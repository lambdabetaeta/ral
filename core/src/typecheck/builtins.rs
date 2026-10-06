//! Type-checker side of the builtin registry.
//!
//! Here live the typing rule each `builtin_registry!` entry in
//! `core/src/builtins.rs` selects through its `ty:` field, the scheme
//! factories it picks from — base frames included, which name one directly —
//! and the record shapes the record-valued builtins share.
//!
//! A scheme factory allocates its unifier vars fresh, so the scheme it returns
//! needs no renaming pass.

use super::env::TyEnv;
use super::generalize::{FreeVars, generalize};
use super::unify::Unifier;
use crate::fact::{ErrorRecord, Io, Observation, Receipt};
use crate::path::walk::{DirEntry, FileInfo};
use crate::ty::{
    CachedFreeVars, CompTy, Grade, GradeVar, Kind, Label, Row, RowVar, Scheme, Ty, TyVar, Typed,
    WeakVars, closed_record, closed_variant,
};
use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

/// How the type checker types a registered builtin: a scheme factory, run
/// fresh against a live [`Unifier`].
///
/// A base frame names one directly, the same as any value builtin — an argv
/// is one argument of one type, not a list of slots to diagnose one by one.
pub(crate) type BuiltinTypeRule = fn(&mut Unifier) -> Scheme;

/// Extra non-typing behaviour a builtin's [`Decl`] carries: which diagnostic
/// an over-application or a literal misuse earns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinDiagnostic {
    None,
    FailStatusNonzero,
    /// A `from-*` decoder: an argument is not an arity slip but a misreading
    /// of where the bytes come from.
    Decoder,
}

/// Which of ral's two argument conventions a manifest row uses.  The manifest
/// is authored as two, and what a name can do follows from which half it is in
/// rather than from its arity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convention {
    /// Curried application at the arity the row's type declares, and
    /// first-class as `$name`: the row seeds the base env scope as a
    /// `Value::Native`.
    Value,
    /// An argv, so the row seeds a base handler frame instead: intercepted,
    /// stacked, reached by `^name`, and never a value.  Typed `List String`,
    /// an argv's elements crossing rendered — though the body is handed the
    /// values themselves, and renders what it writes (`echo`) or vets what it
    /// launches (`detach`) as its own boundary demands.
    Argv,
}

/// `true` and `false` — language-given names in every base scope, live and
/// hydrated alike, though they are not manifest entries.  The checker types
/// them `Bool`.
pub(crate) const LANGUAGE_CONSTANTS: [(&str, bool); 2] = [("true", true), ("false", false)];

/// What a row's type rule fixes once: its curry depth and whether it settles
/// at `Unit`.  One instantiation serves both.
#[derive(Debug, Clone, Copy)]
struct Spine {
    arity: usize,
    settles_at_unit: bool,
}

impl Spine {
    fn of(rule: BuiltinTypeRule) -> Self {
        fn settled(ct: &CompTy) -> (usize, Option<&Ty>) {
            match ct {
                CompTy::Fun(_, body) => {
                    let (n, ty) = settled(body);
                    (n + 1, ty)
                }
                CompTy::Return(_, ty) => (0, Some(ty)),
                CompTy::Var(_) => (0, None),
            }
        }
        match rule(&mut Unifier::new()).ty {
            Ty::Thunk(inner) => {
                let (arity, ty) = settled(&inner);
                Self {
                    arity,
                    settles_at_unit: matches!(ty, Some(Ty::Unit)),
                }
            }
            _ => Self {
                arity: 0,
                settles_at_unit: false,
            },
        }
    }
}

/// A manifest row minus its body: all the checker may know of a builtin.
/// `doc` is the line `help` and `explain` print.
#[derive(Debug, Clone)]
pub struct Decl {
    pub name: Cow<'static, str>,
    pub convention: Convention,
    pub doc: &'static str,
    pub(crate) type_rule: BuiltinTypeRule,
    /// Which diagnostic an over-application or a literal misuse earns; `None`
    /// for the overwhelming majority of rows.
    pub(crate) diagnostic: BuiltinDiagnostic,
    /// Set only by the boundary constructor: a call of the row carries a
    /// [`crate::ty::Site`].
    pub(crate) boundary: bool,
    spine: OnceLock<Spine>,
}

impl Decl {
    pub(crate) const fn new(
        name: Cow<'static, str>,
        convention: Convention,
        type_rule: BuiltinTypeRule,
        doc: &'static str,
        boundary: bool,
    ) -> Self {
        Self {
            name,
            convention,
            doc,
            type_rule,
            diagnostic: BuiltinDiagnostic::None,
            boundary,
            spine: OnceLock::new(),
        }
    }

    pub fn is_boundary(&self) -> bool {
        self.boundary
    }

    fn spine(&self) -> Spine {
        *self.spine.get_or_init(|| Spine::of(self.type_rule))
    }

    /// The curry depth of the row's type: the argument count a `$name`
    /// reference saturates at, and what the checker's arity diagnostics at
    /// command position read.
    pub(crate) fn fixed_arity(&self) -> usize {
        self.spine().arity
    }

    /// The declared scheme is the authority on what a row settles to.
    pub(crate) fn settles_at_unit(&self) -> bool {
        self.spine().settles_at_unit
    }
}

/// The checker's Σ: every installed set's declarations, newest set first.
/// Names are disjoint across sets, so lookup order never shadows.
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    sets: imbl::Vector<Arc<[Decl]>>,
}

impl Manifest {
    /// Panics when `set` repeats a name or collides with an installed one.
    pub(crate) fn push(&mut self, set: Arc<[Decl]>) {
        let mut seen = std::collections::HashSet::new();
        for decl in set.iter() {
            let name = decl.name.as_ref();
            assert!(
                seen.insert(name),
                "builtin installation failed: builtin `{name}` is installed twice in one builtin set"
            );
            assert!(
                self.get(name).is_none(),
                "builtin installation failed: builtin `{name}` conflicts with an installed builtin"
            );
        }
        self.sets.push_back(set);
    }

    /// Whether this exact set, or one carrying the same names, is installed.
    pub(crate) fn holds(&self, set: &[Decl]) -> bool {
        self.sets.iter().any(|have| {
            have.len() == set.len() && set.iter().all(|d| have.iter().any(|h| h.name == d.name))
        })
    }

    fn rows(&self) -> impl Iterator<Item = &Decl> {
        self.sets.iter().rev().flat_map(|set| set.iter())
    }

    /// Any row by name, either half: what `help` and `explain` document.
    pub fn get(&self, name: &str) -> Option<&Decl> {
        self.rows().find(|decl| decl.name == name)
    }

    /// The *value* row for `name`: the half an application and a `$name`
    /// reference reach.  `None` for a base frame, which command position and
    /// `^name` reach through the handler stack instead.
    pub fn value(&self, name: &str) -> Option<&Decl> {
        self.rows()
            .find(|decl| decl.name == name && decl.convention == Convention::Value)
    }

    /// Every base-frame row: what the checker's handler bindings are seeded
    /// from.
    pub(crate) fn base_frames(&self) -> impl Iterator<Item = &Decl> {
        self.rows()
            .filter(|decl| decl.convention == Convention::Argv)
    }

    /// Names a `$name` reference reaches: the value rows.
    pub(crate) fn value_names(&self) -> impl Iterator<Item = &str> {
        self.rows()
            .filter(|decl| decl.convention == Convention::Value)
            .map(|decl| decl.name.as_ref())
    }

    /// Names of installed builtins, newest installed set first.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.rows().map(|decl| decl.name.as_ref())
    }
}

/// Build a [`Scheme`] from its quantified vars and body.
///
/// Each type variable comes with its [`Kind`], each row variable with whether
/// it is deep.  Public so host crates can write their own scheme arms without
/// touching `Scheme`'s internals.
pub fn mk_scheme(ty_vars: &[(TyVar, Kind)], row_vars: &[(RowVar, bool)], ty: Ty) -> Scheme {
    Scheme {
        ty_vars: ty_vars.to_vec(),
        comp_ty_vars: vec![],
        row_vars: row_vars.to_vec(),
        grade_vars: vec![],
        ty,
        comp_ty_bindings: vec![],
        ty_bindings: vec![],
        cached_fv: Some(CachedFreeVars::default()),
        weak: WeakVars::default(),
    }
}

/// [`mk_scheme`] over unconstrained variables and shallow rows.
pub fn mk_plain_scheme(ty_vars: &[TyVar], row_vars: &[RowVar], ty: Ty) -> Scheme {
    let kinded: Vec<_> = ty_vars.iter().map(|&v| (v, Kind::ANY)).collect();
    let rows: Vec<_> = row_vars.iter().map(|&v| (v, false)).collect();
    mk_scheme(&kinded, &rows, ty)
}

pub fn thunk(cty: CompTy) -> Ty {
    Ty::Thunk(Box::new(cty))
}
pub fn fun(param: Ty, body: CompTy) -> CompTy {
    CompTy::Fun(Box::new(param), Box::new(body))
}
pub fn pure(ty: Ty) -> CompTy {
    CompTy::pure(ty)
}
pub fn command() -> CompTy {
    CompTy::command()
}

/// `scheme`, quantifying `grades` too.  Public, as [`mk_scheme`] is: a host
/// row that absorbs a block names the grade it runs at.
pub fn graded(grades: &[GradeVar], scheme: Scheme) -> Scheme {
    Scheme {
        grade_vars: grades.to_vec(),
        ..scheme
    }
}

// ── Scheme DSL ──────────────────────────────────────────────────────
//
// `scheme!` writes a builtin's polytype declaratively: `<tv>` declares fresh
// type vars, `[...]` params curry left-to-right, `pure` means a thunked
// constant, and `writes` a command in place of a result.  Each arm is
// labelled with the spelling it accepts.
//
// It expands only inside `mod scheme` below, whose imports are what the
// expansion resolves against.
//
// The arms expand to `pub fn`, deliberately: this one family sits outside the
// `pub(crate)`-by-default discipline
// (`docs/ral-wiki/decisions/260909_pub-crate-by-default.md`).  Per-arm
// visibility would mean threading a `$vis` through every call site to narrow
// members that a base frame's row already names — and a row is a caller, so no
// member of this family can be stranded silently the way a loose function can.

macro_rules! scheme {
    // scheme!(help: writes);
    ($name:ident: writes) => { scheme!(@ $name: [] => command()); };
    // scheme!(temp_path: pure Ty::String);
    ($name:ident: pure $ret:expr) => { scheme!(@ $name: [] => pure($ret)); };
    // scheme!(to_bytes: [Ty::Bytes] -> writes);
    ($name:ident: [$($p:expr),*] -> writes) => { scheme!(@ $name: [$($p),*] => command()); };
    // scheme!(str_to_str: [Ty::String] -> Ty::String);
    ($name:ident: [$($p:expr),*] -> $ret:expr) => { scheme!(@ $name: [$($p),*] => pure($ret)); };
    // scheme!(to_line<av: Kind::DATA>: [Ty::Var(av)] -> writes);
    ($name:ident<$($tv:ident: $kind:path),+>: [$($p:expr),*] -> writes) => {
        scheme!(@ $name<$($tv: $kind),+>: [$($p),*] => command());
    };
    // scheme!(length<av: Kind::SIZED>: [Ty::Var(av)] -> Ty::Int);
    // scheme!(has<av: Kind::ANY, bv: Kind::ANY>: [..] -> ..): any number.
    ($name:ident<$($tv:ident: $kind:path),+>: [$($p:expr),*] -> $ret:expr) => {
        scheme!(@ $name<$($tv: $kind),+>: [$($p),*] => pure($ret));
    };
    // The two expansions: `tail` past the curried parameters, under no type
    // variable, or one of the kind declared.
    (@ $name:ident: [$($p:expr),*] => $tail:expr) => {
        pub fn $name(_u: &mut Unifier) -> Scheme {
            mk_plain_scheme(&[], &[], thunk(CompTy::arrows([$($p),*], $tail)))
        }
    };
    (@ $name:ident<$($tv:ident: $kind:path),+>: [$($p:expr),*] => $tail:expr) => {
        pub fn $name(u: &mut Unifier) -> Scheme {
            $(let $tv = u.fresh_tyvar();)+
            mk_scheme(&[$(($tv, $kind)),+], &[], thunk(CompTy::arrows([$($p),*], $tail)))
        }
    };
}

/// The error record a raising form demands of its argument: `status` and
/// `message`, over a fresh tail.  Open, because re-raising a caught error
/// carries `cmd`, `site` and whatever else the record picked up along
/// the way; the tail is what makes `ErrorRecord::ty` an instance of this
/// shape.  The caller quantifies the row itself — [`scheme::fail`] — so this
/// takes it directly rather than minting one.
pub(in crate::typecheck) fn error_record_shape(row: RowVar) -> Ty {
    Ty::Record(Row::Extend(
        Label::Field("status".into()),
        Box::new(Ty::Int),
        Box::new(Row::Extend(
            Label::Field("message".into()),
            Box::new(Ty::String),
            Box::new(Row::Var(row)),
        )),
    ))
}

/// The record `audit { … }` produces, field for field the value
/// `report_value` materialises.  The body's outcome is a variant, so a failure
/// is read by `case` rather than by comparing a status against 0; the
/// `` `err `` payload is the very record `try` hands its handler.
pub(super) fn audit_record(value_ty: Ty) -> Ty {
    closed_record(&[
        ("outcome", outcome_ty(value_ty)),
        ("trail", Vec::<Observation>::ty()),
    ])
}

/// The `` `ok α | `err `` a body's outcome is read as: the failure is the
/// record `try` hands its handler.
pub(crate) fn outcome_ty(value_ty: Ty) -> Ty {
    closed_variant(&[("ok", value_ty), ("err", ErrorRecord::ty())])
}

/// The `{value, stdout, stderr}` record `await` and `race` return.  Failure
/// raises rather than setting a flag, so there is no status field here; a
/// failed block's status lives inside `poll`'s `` `err `` outcome.
pub(crate) fn await_record(value_ty: Ty) -> Ty {
    Io::ty().extend("value", value_ty)
}

/// The `{stdout, stderr, outcome}` record `poll` carries in its `` `settled ``
/// arm.  The `` `err `` payload is the very record `try` hands its handler,
/// so the block's status lives inside it.
fn settle_record(value_ty: Ty) -> Ty {
    Io::ty().extend("outcome", outcome_ty(value_ty))
}

/// The variant `poll` returns: [`settle_record`] once the block has finished —
/// by returning, raising, or panicking — and what it has written so far while
/// it runs (a cumulative, non-destructive snapshot).  Being `await`'s
/// non-blocking dual, `poll` reports a failure inside the settled outcome
/// rather than re-raising it.
pub(crate) fn poll_variant(value_ty: Ty) -> Ty {
    closed_variant(&[("pending", Io::ty()), ("settled", settle_record(value_ty))])
}

/// Per-builtin scheme factories, one function per registered *shape*: entries
/// that share one (`upper`, `lower`, `dedent`, `ral-quote`) reuse a single
/// function here rather than duplicating the body.
pub mod scheme {
    use super::{
        CompTy, DirEntry, FileInfo, FreeVars, Grade, GradeVar, Kind, Receipt, Row, Scheme, Ty,
        TyEnv, TyVar, Unifier, await_record, command, error_record_shape, fun, generalize, graded,
        mk_plain_scheme, mk_scheme, poll_variant, pure, thunk,
    };
    use crate::ty::Typed as _;

    // ── List operations ──────────────────────────────────────────────────

    scheme!(length<av: Kind::SIZED>: [Ty::Var(av)] -> Ty::Int);

    /// `surface :: ∀ρ. Variant ρ → F ()` — forward a tagged event to the host's
    /// event sink.  The row stays open: the host decides which tags it knows.
    ///
    /// `pub`: a host declares its own [`BuiltinEntry`](crate::types::BuiltinEntry)
    /// over this scheme under its own name; see
    /// [`crate::builtins::SURFACE_BUILTIN`].
    pub fn surface_op(u: &mut Unifier) -> Scheme {
        let row = u.fresh_row_var();
        mk_plain_scheme(
            &[],
            &[row],
            thunk(fun(Ty::Variant(Row::Var(row)), pure(Ty::Unit))),
        )
    }

    // keys :: ∀α. Map<α> → F [Str]
    scheme!(keys<av: Kind::ANY>: [Ty::map(Ty::Var(av))] -> Ty::list(Ty::String));

    // has :: ∀α. Map<α> → Str → F Bool
    scheme!(has<av: Kind::ANY>: [Ty::map(Ty::Var(av)), Ty::String] -> Ty::Bool);

    // map :: ∀α β. U(α → F β) → [α] → F [β]
    scheme!(map_op<av: Kind::ANY, bv: Kind::ANY>: [
        thunk(fun(Ty::Var(av), pure(Ty::Var(bv)))),
        Ty::list(Ty::Var(av))
    ] -> Ty::list(Ty::Var(bv)));

    // filter :: ∀α. U(α → F Bool) → [α] → F [α]
    scheme!(filter_op<av: Kind::ANY>: [
        thunk(fun(Ty::Var(av), pure(Ty::Bool))),
        Ty::list(Ty::Var(av))
    ] -> Ty::list(Ty::Var(av)));

    /// `each :: ∀ε α β. U(α → F^ε β) → [α] → F Unit` — the block's output
    /// streams, whatever it produces.
    pub(crate) fn each_op(u: &mut Unifier) -> Scheme {
        let (av, bv, ev) = (u.fresh_tyvar(), u.fresh_tyvar(), u.fresh_grade_var());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        graded(
            &[ev],
            mk_plain_scheme(
                &[av, bv],
                &[],
                thunk(fun(
                    thunk(fun(a.clone(), CompTy::Return(Grade::Var(ev), Box::new(b)))),
                    fun(Ty::list(a), pure(Ty::Unit)),
                )),
            ),
        )
    }

    /// `fold :: ∀ε α β. U(β → α → F^ε β) → β → [α] → F β`
    pub(crate) fn fold_op(u: &mut Unifier) -> Scheme {
        let (av, bv, ev) = (u.fresh_tyvar(), u.fresh_tyvar(), u.fresh_grade_var());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        let step = CompTy::Return(Grade::Var(ev), Box::new(b.clone()));
        graded(
            &[ev],
            mk_plain_scheme(
                &[av, bv],
                &[],
                thunk(fun(
                    thunk(fun(b.clone(), fun(a.clone(), step))),
                    fun(b.clone(), fun(Ty::list(a), pure(b))),
                )),
            ),
        )
    }

    // sort-list-by :: ∀α β:comparable. U(α → F β) → [α] → F [α]
    scheme!(sort_list_by<av: Kind::ANY, bv: Kind::COMPARABLE>: [
        thunk(fun(Ty::Var(av), pure(Ty::Var(bv)))),
        Ty::list(Ty::Var(av))
    ] -> Ty::list(Ty::Var(av)));

    // ── Strings & paths ──────────────────────────────────────────────────

    scheme!(str_to_str: [Ty::String] -> Ty::String);

    scheme!(str_to_strs: [Ty::String] -> Ty::list(Ty::String));

    scheme!(re_match: [Ty::String, Ty::String] -> Ty::Bool);

    scheme!(re_find_match: [Ty::String, Ty::String] -> Ty::String);

    scheme!(re_split: [Ty::String, Ty::String] -> Ty::list(Ty::String));

    scheme!(replace_3: [Ty::String, Ty::String, Ty::String] -> Ty::String);

    scheme!(slice: [Ty::String, Ty::Int, Ty::Int] -> Ty::String);

    // intercalate :: ∀α. Str → [α] → F Str
    scheme!(intercalate<av: Kind::ANY>: [Ty::String, Ty::list(Ty::Var(av))] -> Ty::String);

    // ── File system & paths ──────────────────────────────────────────────

    // list-dir :: Str → F [{name, type, size, mtime}]
    scheme!(list_dir: [Ty::String] -> Ty::list(DirEntry::ty()));

    // file-info :: Str → F {…full stat…}
    scheme!(file_info: [Ty::String] -> FileInfo::ty());

    scheme!(temp_path: pure Ty::String);

    scheme!(glob: [Ty::String] -> Ty::list(Ty::String));

    // ── Streaming reducers ───────────────────────────────────────────────

    /// `fold-lines :: ∀ε α. U(α → Str → F^ε α) → α → F α`
    pub(crate) fn fold_lines(u: &mut Unifier) -> Scheme {
        let (av, ev) = (u.fresh_tyvar(), u.fresh_grade_var());
        let a = Ty::Var(av);
        let step = CompTy::Return(Grade::Var(ev), Box::new(a.clone()));
        graded(
            &[ev],
            mk_plain_scheme(
                &[av],
                &[],
                thunk(fun(
                    thunk(fun(a.clone(), fun(Ty::String, step))),
                    fun(a.clone(), pure(a)),
                )),
            ),
        )
    }

    // ── Concurrency ──────────────────────────────────────────────────────

    /// `∀ε α. U(F^ε α) → F (Handle α)`, behind `prefix` leading parameters:
    /// the block runs in a worker, its output streamed, whatever it produces.
    fn runs_block(u: &mut Unifier, prefix: &[Ty]) -> Scheme {
        let (av, ev) = (u.fresh_tyvar(), u.fresh_grade_var());
        let a = Ty::Var(av);
        let body = CompTy::Return(Grade::Var(ev), Box::new(a.clone()));
        let run = fun(thunk(body), pure(Ty::Handle(Box::new(a))));
        let spine = prefix
            .iter()
            .rev()
            .fold(run, |body, p| fun(p.clone(), body));
        graded(&[ev], mk_plain_scheme(&[av], &[], thunk(spine)))
    }

    /// `spawn :: ∀ε α. U(F^ε α) → F (Handle α)`
    pub fn spawn(u: &mut Unifier) -> Scheme {
        runs_block(u, &[])
    }

    /// `watch :: ∀ε α. String → U(F^ε α) → F (Handle α)`
    pub(crate) fn watch(u: &mut Unifier) -> Scheme {
        runs_block(u, &[Ty::String])
    }

    /// `service :: ∀ε α. String → U(F^ε α) → F (Handle α)` — `watch`'s
    /// scheme, the leading `String` being the mandatory birth description.
    ///
    /// The durable lease class is a runtime fact, invisible to the types.
    pub(crate) fn service(u: &mut Unifier) -> Scheme {
        runs_block(u, &[Ty::String])
    }

    /// `await :: ∀α. Handle α → F {value, stdout, stderr}`
    pub(crate) fn await_op(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        mk_plain_scheme(
            &[av],
            &[],
            thunk(fun(Ty::Handle(Box::new(a.clone())), pure(await_record(a)))),
        )
    }

    /// `poll :: ∀α. Handle α → F <pending: {stdout, stderr} | settled: {stdout, stderr, outcome: <ok: α | err: ErrRecord>}>`
    pub(crate) fn poll(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        mk_plain_scheme(
            &[av],
            &[],
            thunk(fun(Ty::Handle(Box::new(a.clone())), pure(poll_variant(a)))),
        )
    }

    /// `race :: ∀α. [Handle α] → F {value, stdout, stderr}`
    pub(crate) fn race(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        mk_plain_scheme(
            &[av],
            &[],
            thunk(fun(
                Ty::list(Ty::Handle(Box::new(a.clone()))),
                pure(await_record(a)),
            )),
        )
    }

    /// `cancel :: ∀α. Handle α → F Unit`
    pub(crate) fn cancel_op(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        mk_plain_scheme(
            &[av],
            &[],
            thunk(fun(Ty::Handle(Box::new(Ty::Var(av))), pure(Ty::Unit))),
        )
    }

    // ── Base frames ──────────────────────────────────────────────────────

    /// A base frame's type: an argv in, the computation the frame names out.
    /// One shape for the whole argv half of the manifest — a frame differs
    /// from its siblings only in the result it names.
    fn base_frame(ty_vars: &[TyVar], result: CompTy) -> Scheme {
        mk_plain_scheme(ty_vars, &[], thunk(fun(Ty::argv(), result)))
    }

    /// `echo :: [Str] → Command` — join the argv with single spaces and write it
    /// with a trailing newline.
    pub(crate) fn echo(_u: &mut Unifier) -> Scheme {
        base_frame(&[], command())
    }

    /// `detach :: [Str] → F [pid: Int, desc: Str]` — the receipt of a process this
    /// session stops owning.
    pub fn detach(_u: &mut Unifier) -> Scheme {
        base_frame(&[], pure(Receipt::ty()))
    }

    // ── First-class constants / queries ──────────────────────────────────

    scheme!(pure_string: pure Ty::String);

    scheme!(pure_int: pure Ty::Int);

    scheme!(pure_strs: pure Ty::list(Ty::String));

    scheme!(pure_string_map: pure Ty::map(Ty::String));

    scheme!(pure_bool: pure Ty::Bool);

    // ── Host-backed queries ───────────────────────────────────────────────

    scheme!(ask: [Ty::String] -> Ty::String);

    /// `use :: ∀ρ. Str → F [ | ρ]` — an open record, since a module's bindings
    /// are heterogeneous and reached by name.
    pub fn use_op(u: &mut Unifier) -> Scheme {
        let rv = u.fresh_row_var();
        mk_plain_scheme(
            &[],
            &[rv],
            thunk(fun(Ty::String, pure(Ty::Record(Row::Var(rv))))),
        )
    }

    // ── Terminal, help & encoders ────────────────────────────────────────
    //
    // Each is a command: its value is what it writes.

    scheme!(terminal_control: writes);
    scheme!(help: writes);
    scheme!(explain: [Ty::String] -> writes);
    scheme!(to_bytes: [Ty::Bytes] -> writes);
    scheme!(ints_to_bytes: [Ty::list(Ty::Int)] -> writes);
    scheme!(to_any_bytes<av: Kind::DATA>: [Ty::Var(av)] -> writes);
    scheme!(to_line<av: Kind::DATA>: [Ty::Var(av)] -> writes);
    scheme!(to_lines<av: Kind::DATA>: [Ty::list(Ty::Var(av))] -> writes);
    scheme!(to_csv: [Ty::list(Ty::Map(Box::new(Ty::String)))] -> writes);

    // ── Decoders ─────────────────────────────────────────────────────────
    //
    // Nullary: the bytes come from the channel, not an argument.

    scheme!(from_bytes: pure Ty::Bytes);
    scheme!(from_string: pure Ty::String);
    scheme!(from_lines: pure Ty::list(Ty::String));
    scheme!(from_csv: pure Ty::list(Ty::Map(Box::new(Ty::String))));

    /// `from-json` :: ∀α. F α — decode whatever the channel holds.
    pub fn from_json(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        mk_plain_scheme(&[av], &[], thunk(pure(Ty::Var(av))))
    }

    /// `from-json-at` :: ∀α. [String] → F α — the value at a path, whatever it
    /// holds.
    pub fn from_json_at(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let tokens = Ty::list(Ty::String);
        mk_plain_scheme(&[av], &[], thunk(fun(tokens, pure(Ty::Var(av)))))
    }

    /// `from-jsonl` :: ∀α. F [α] — a list of whatever each line holds.
    pub fn from_jsonl(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let records = Ty::list(Ty::Var(av));
        mk_plain_scheme(&[av], &[], thunk(pure(records)))
    }

    // ── Range, paths, parsing ────────────────────────────────────────────

    scheme!(range: [Ty::Int, Ty::Int] -> Ty::list(Ty::Int));
    scheme!(chdir: [Ty::String] -> Ty::Unit);
    scheme!(path_bool: [Ty::String] -> Ty::Bool);
    scheme!(int_parse<av: Kind::COMPARABLE>: [Ty::Var(av)] -> Ty::Int);
    scheme!(float_parse<av: Kind::COMPARABLE>: [Ty::Var(av)] -> Ty::Float);
    scheme!(str_parse<av: Kind::DATA>: [Ty::Var(av)] -> Ty::String);
    scheme!(round: [Ty::Float, Ty::Int] -> Ty::Float);
    // Shared by `floor`, `ceil`, and `trunc`.
    scheme!(float_to_int: [Ty::Float] -> Ty::Int);

    // ── Aliasing, job control, plugins ───────────────────────────────────

    /// `alias :: String → U(comp) → F Unit`.
    ///
    /// The block's own computation type is unconstrained here (only its
    /// arity is checked elsewhere), so the comp var it mints must be
    /// quantified through [`generalize`]: [`mk_scheme`] cannot quantify one,
    /// its `comp_ty_bindings` being always empty.
    pub(crate) fn alias(u: &mut Unifier) -> Scheme {
        let block = u.fresh_comp_ty();
        let body = fun(Ty::String, fun(thunk(block), pure(Ty::Unit)));
        generalize(u, &TyEnv::new(), &FreeVars::new(), &thunk(body))
    }
    scheme!(unalias: [Ty::String] -> Ty::Unit);

    scheme!(string_to_unit: [Ty::String] -> Ty::Unit);

    // ── Divergence ───────────────────────────────────────────────────────

    /// `∀ε α. F^ε α`: a divergent computation inhabits every producer type,
    /// so it joins whatever the other arm of an `if` is, a command included.
    fn divergent(u: &mut Unifier) -> (TyVar, GradeVar, CompTy) {
        let (av, ev) = (u.fresh_tyvar(), u.fresh_grade_var());
        (
            av,
            ev,
            CompTy::Return(Grade::Var(ev), Box::new(Ty::Var(av))),
        )
    }

    /// `fail :: ∀ε α r. {status: Int, message: String | r} → F^ε α`.
    ///
    /// An error record, open at the tail so a caught error re-raises with the
    /// fields `try` gave it.
    pub(crate) fn fail(u: &mut Unifier) -> Scheme {
        let row = u.fresh_row_var();
        let (av, ev, never) = divergent(u);
        graded(
            &[ev],
            mk_plain_scheme(&[av], &[row], thunk(fun(error_record_shape(row), never))),
        )
    }

    /// `exit`/`quit` :: ∀ε α. Int → F^ε α — a status, and no return.
    /// The elaborator sugars bare `exit` to `exit 0`.
    pub(crate) fn exit(u: &mut Unifier) -> Scheme {
        let (av, ev, never) = divergent(u);
        graded(
            &[ev],
            mk_plain_scheme(&[av], &[], thunk(fun(Ty::Int, never))),
        )
    }

    /// `∀ε α. F^ε α` — nullary; for a host builtin that never returns (a
    /// test-only Rust panic trigger, say).
    pub fn diverges(u: &mut Unifier) -> Scheme {
        let (av, ev, never) = divergent(u);
        graded(&[ev], mk_plain_scheme(&[av], &[], thunk(never)))
    }
}

/// The formatted type of any manifest row, either half.
///
/// What `help` and `explain` print: a base frame's argv type is worth printing
/// even though no `$name` can hold it.  `None` only for a name the manifest
/// does not carry.
pub fn builtin_type_hint(manifest: &Manifest, name: &str) -> Option<String> {
    let mut u = Unifier::new();
    Some((manifest.get(name)?.type_rule)(&mut u).to_string())
}

/// Detect the literal `fail [status: 0, …]` shape, so the nonzero-status rule
/// `builtins::misc::builtin_fail` enforces at runtime can be diagnosed at
/// typecheck time.
///
/// Computed statuses and spreads still defer to the runtime.
pub(crate) fn fail_status_is_zero_literal(args: &crate::ir::Args) -> bool {
    let Some(positional) = args.positional() else {
        return false;
    };
    matches!(
        positional.first(),
        Some(crate::ir::Val::Record(entries)) if entries.shape().iter().any(|(k, v)| {
            k.as_ref() == "status" && matches!(v.item, crate::ir::Val::Int(0))
        })
    )
}

#[cfg(test)]
mod tests {
    /// A `[arg]` in a doc synopsis (before the em dash) reads as optional, and
    /// no argument is: every template is one the caller must write.  A bracket
    /// spelling a record type — `[status: Int, ...]`, one required argument —
    /// is not that: only a field-free bracket reads as an optional argument.
    /// `exit`/`quit` are exempt: `[status]` is elaborator sugar over a fixed-1
    /// form.
    #[test]
    fn no_builtin_doc_reads_as_optional() {
        let manifest = crate::HostSurface::default().manifest();
        for name in manifest.names() {
            if name == "exit" || name == "quit" {
                continue;
            }
            let entry = manifest.get(name).unwrap();
            let synopsis = entry.doc.split('—').next().unwrap_or(entry.doc);
            let optional_looking = synopsis.split('[').skip(1).any(|rest| {
                let bracketed = rest.split(']').next().unwrap_or(rest);
                !bracketed.contains(':')
            });
            assert!(
                !optional_looking,
                "builtin '{name}': doc reads as optional, but every template is required"
            );
        }
    }
}
