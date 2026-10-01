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
use super::fmt::fmt_scheme;
use super::generalize::{FreeVars, generalize};
use super::kind::Kind;
use super::scheme::{CachedFreeVars, Scheme, WeakVars};
use super::ty::{CompTy, Grade, GradeVar, Label, Row, RowVar, Ty, TyVar};
use super::unify::Unifier;
use crate::types::BuiltinTable;

/// How the type checker types a registered builtin: a scheme factory, run
/// fresh against a live [`Unifier`].
///
/// A base frame names one directly, the same as any value builtin — an argv
/// is one argument of one type, not a list of slots to diagnose one by one.
pub(crate) type BuiltinTypeRule = fn(&mut Unifier) -> Scheme;

/// The `Fun`-nesting depth of a scheme factory's curried body — instantiated
/// fresh, since a factory needs a live [`Unifier`] to run.  A builtin's
/// arity: the checker's own arity diagnostics at command position read this,
/// not only the evaluator's arity gate.
pub(crate) fn scheme_curry_depth(factory: BuiltinTypeRule) -> usize {
    let mut u = Unifier::new();
    let scheme = factory(&mut u);
    fn count(ct: &CompTy) -> usize {
        match ct {
            CompTy::Fun(_, body) => 1 + count(body),
            _ => 0,
        }
    }
    match &scheme.ty {
        Ty::Thunk(inner) => count(inner),
        _ => 0,
    }
}

/// Extra non-typing behaviour a builtin's [`crate::types::BuiltinEntry`]
/// carries: which diagnostic an over-application or a literal misuse earns.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinDiagnostic {
    None,
    FailStatusNonzero,
    /// A `from-*` decoder: an argument is not an arity slip but a misreading
    /// of where the bytes come from.
    Decoder,
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
    ($name:ident<$tv:ident: $kind:path>: [$($p:expr),*] -> writes) => {
        scheme!(@ $name<$tv: $kind>: [$($p),*] => command());
    };
    // scheme!(length<av: Kind::SIZED>: [Ty::Var(av)] -> Ty::Int);
    ($name:ident<$tv:ident: $kind:path>: [$($p:expr),*] -> $ret:expr) => {
        scheme!(@ $name<$tv: $kind>: [$($p),*] => pure($ret));
    };
    // The two expansions: `tail` past the curried parameters, under no type
    // variable, or one of the kind declared.
    (@ $name:ident: [$($p:expr),*] => $tail:expr) => {
        pub fn $name(_u: &mut Unifier) -> Scheme {
            mk_plain_scheme(&[], &[], thunk(CompTy::arrows([$($p),*], $tail)))
        }
    };
    (@ $name:ident<$tv:ident: $kind:path>: [$($p:expr),*] => $tail:expr) => {
        pub fn $name(u: &mut Unifier) -> Scheme {
            let $tv = u.fresh_tyvar();
            mk_scheme(&[($tv, $kind)], &[], thunk(CompTy::arrows([$($p),*], $tail)))
        }
    };
}

/// A record type over a row of fields ending in `tail`.
pub fn record_row(fields: &[(&str, Ty)], tail: Row) -> Ty {
    let mut row = tail;
    for (l, t) in fields.iter().rev() {
        row = Row::Extend(
            Label::Field((*l).to_string()),
            Box::new(t.clone()),
            Box::new(row),
        );
    }
    Ty::Record(row)
}

/// A record type over a closed row: the tail is `Empty`, so no extension.
pub fn closed_record(fields: &[(&str, Ty)]) -> Ty {
    record_row(fields, Row::Empty)
}

/// A record type left open on `tail`: at least these fields, and any others.
pub fn open_record(fields: &[(&str, Ty)], tail: RowVar) -> Ty {
    record_row(fields, Row::Var(tail))
}

/// A variant type over a row of tags with stated payloads, ending in `tail`.
pub fn variant_row(tags: &[(&str, Ty)], tail: Row) -> Ty {
    let mut row = tail;
    for (l, t) in tags.iter().rev() {
        row = Row::Extend(
            Label::Case((*l).to_string()),
            Box::new(t.clone()),
            Box::new(row),
        );
    }
    Ty::Variant(row)
}

/// A variant type over a closed row of tags.
///
/// The tail is `Empty`, so a `case` on it must cover exactly these arms.  A
/// payload-less tag takes `Unit`, as `Inferencer::infer_val` gives one at its
/// construction site.
pub fn closed_variant(arms: &[(&str, Ty)]) -> Ty {
    variant_row(arms, Row::Empty)
}

/// A variant left open on `tail`, so an unknown tag reaches the runtime door
/// that enumerates the legal ones rather than dying as a row-unification
/// mismatch.
pub fn open_variant(tags: &[(&str, Ty)], tail: RowVar) -> Ty {
    variant_row(tags, Row::Var(tail))
}

/// The error record a raising form demands of its argument: `status` and
/// `message`, over a fresh tail.  Open, because re-raising a caught error
/// carries `cmd`, `site` and whatever else the record picked up along
/// the way; the tail is what makes [`try_error_record`] an instance of this
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

/// The error record `try` hands its handler, mirrored at runtime by
/// `error_record` in `core/src/evaluator/scope.rs`.  `message` is synthetic
/// status text, never the failing command's fd 2 bytes: those streamed live,
/// and `audit` is the forensic path.
pub(super) fn try_error_record() -> Ty {
    closed_record(&[
        ("status", Ty::Int),
        ("reason", reason_ty()),
        ("cmd", Ty::String),
        ("message", Ty::String),
        ("site", site_ty()),
    ])
}

/// The `` `just x | `none `` an optional projected field carries: an absent
/// before-image is a fact, not a missing key.
fn optional_ty(payload: Ty) -> Ty {
    closed_variant(&[("just", payload), ("none", Ty::Unit)])
}

/// Why a failure happened, mirrored at runtime by `reason_value` in
/// `core/src/evaluator/scope.rs`.
fn reason_ty() -> Ty {
    let causes = crate::process::CancelCause::ALL.map(|c| (c.label(), Ty::Unit));
    let cause = closed_variant(&causes);
    closed_variant(&[
        ("exited", Ty::Int),
        ("signaled", Ty::Int),
        ("cancelled", cause),
        ("not-found", Ty::Unit),
        ("not-runnable", Ty::Unit),
        ("raised", Ty::Unit),
    ])
}

/// A source position, mirrored at runtime by `site_value` in
/// `core/src/types/observation.rs`.
fn site_ty() -> Ty {
    optional_ty(closed_record(&[
        ("script", Ty::String),
        ("line", Ty::Int),
        ("col", Ty::Int),
    ]))
}

/// One arm per [`Observed`](crate::types::Observed) variant, tagged by the
/// kind, each a closed record of exactly the fields `Observation::to_value`
/// projects for it.
fn observed_ty() -> Ty {
    closed_variant(&[
        (
            "command",
            closed_record(&[
                ("argv", Ty::List(Box::new(Ty::String))),
                ("status", Ty::Int),
                ("origin", Ty::String),
                ("stdout", Ty::Bytes),
                ("stderr", Ty::Bytes),
                ("error", Ty::String),
            ]),
        ),
        (
            "write",
            closed_record(&[
                ("path", Ty::String),
                ("mode", Ty::String),
                ("outcome", Ty::String),
                ("new_bytes", optional_ty(Ty::Bytes)),
                ("old_bytes", optional_ty(Ty::Bytes)),
            ]),
        ),
        ("read", closed_record(&[("path", Ty::String)])),
        (
            "grep",
            closed_record(&[("scope", Ty::String), ("pattern", Ty::String)]),
        ),
        (
            "check",
            closed_record(&[
                ("resource", Ty::String),
                ("decision", Ty::String),
                ("fields", Ty::Map(Box::new(Ty::String))),
            ]),
        ),
        (
            "worker",
            closed_record(&[("id", Ty::Int), ("cmd", Ty::String), ("class", Ty::String)]),
        ),
        (
            "act",
            closed_record(&[
                ("verb", Ty::String),
                ("subject", optional_ty(Ty::String)),
                ("payload", Ty::String),
                ("refused", Ty::Bool),
            ]),
        ),
    ])
}

/// The record one observation projects as, field for field
/// `Observation::to_value`: the envelope, and the fact as a tagged `what`.
fn observation_ty() -> Ty {
    closed_record(&[
        ("site", site_ty()),
        ("start", Ty::Int),
        ("end", Ty::Int),
        ("principal", Ty::String),
        ("what", observed_ty()),
    ])
}

/// The record `audit { … }` produces, field for field the value shape
/// `evaluator::audit::report_value` materialises.  The body's outcome is a
/// variant, so a failure is read by `case` rather than by comparing a status
/// against 0; the `` `err `` payload is the very record `try` hands its
/// handler.
pub(super) fn audit_record(value_ty: Ty) -> Ty {
    closed_record(&[
        (
            "outcome",
            closed_variant(&[("ok", value_ty), ("err", try_error_record())]),
        ),
        ("trail", Ty::List(Box::new(observation_ty()))),
    ])
}

/// The `{value, stdout, stderr}` record `await` and `race` return.  Failure
/// raises rather than setting a flag, so there is no status field here; a
/// failed block's status lives inside `poll`'s `` `err `` outcome.
fn await_record(value_ty: Ty) -> Ty {
    closed_record(&[
        ("value", value_ty),
        ("stdout", Ty::Bytes),
        ("stderr", Ty::Bytes),
    ])
}

/// The `{stdout, stderr, outcome}` record `poll` carries in its `` `settled ``
/// arm.  The `` `err `` payload is the very record `try` hands its handler
/// ([`try_error_record`]), so the block's status lives inside it.
fn settle_record(value_ty: Ty) -> Ty {
    closed_record(&[
        ("stdout", Ty::Bytes),
        ("stderr", Ty::Bytes),
        (
            "outcome",
            closed_variant(&[("ok", value_ty), ("err", try_error_record())]),
        ),
    ])
}

/// The `{stdout, stderr}` record `poll` carries in its `` `pending `` arm: a
/// cumulative, non-destructive snapshot of what the running block has written
/// so far, and no outcome, because there is none yet.
fn pending_record() -> Ty {
    closed_record(&[("stdout", Ty::Bytes), ("stderr", Ty::Bytes)])
}

/// The variant `poll` returns: [`settle_record`] once the block has finished —
/// by returning, raising, or panicking — and [`pending_record`] while it runs.
/// Being `await`'s non-blocking dual, `poll` reports a failure inside the
/// settled outcome rather than re-raising it.
fn poll_variant(value_ty: Ty) -> Ty {
    closed_variant(&[
        ("pending", pending_record()),
        ("settled", settle_record(value_ty)),
    ])
}

/// The record type returned by `list-dir` for each directory entry.
pub(crate) fn fs_list_entry_ty() -> Ty {
    closed_record(&[
        ("name", Ty::String),
        ("type", Ty::String),
        ("size", Ty::Int),
        ("mtime", Ty::Int),
    ])
}

/// The record type returned by `file-info`: [`fs_list_entry_ty`]'s fields plus
/// access and birth times, the readonly bit, and the symlink `target` (the
/// empty string for non-symlinks).
pub(crate) fn fs_file_info_ty() -> Ty {
    closed_record(&[
        ("name", Ty::String),
        ("type", Ty::String),
        ("size", Ty::Int),
        ("mtime", Ty::Int),
        ("atime", Ty::Int),
        ("btime", Ty::Int),
        ("readonly", Ty::Bool),
        ("target", Ty::String),
    ])
}

/// Per-builtin scheme factories, one function per registered *shape*: entries
/// that share one (`upper`, `lower`, `dedent`, `shell-quote`) reuse a single
/// function here rather than duplicating the body.
pub mod scheme {
    use super::{
        CompTy, FreeVars, Grade, GradeVar, Kind, Row, Scheme, Ty, TyEnv, TyVar, Unifier,
        await_record, closed_record, command, error_record_shape, fs_file_info_ty,
        fs_list_entry_ty, fun, generalize, graded, mk_plain_scheme, mk_scheme, poll_variant, pure,
        thunk,
    };

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

    /// `keys :: ∀α. Map<α> → F [Str]`
    pub fn keys(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        mk_plain_scheme(
            &[av],
            &[],
            thunk(fun(
                Ty::Map(Box::new(Ty::Var(av))),
                pure(Ty::List(Box::new(Ty::String))),
            )),
        )
    }

    /// `has :: ∀α. Map<α> → Str → F Bool`
    pub(crate) fn has(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        mk_plain_scheme(
            &[av],
            &[],
            thunk(fun(
                Ty::Map(Box::new(Ty::Var(av))),
                fun(Ty::String, pure(Ty::Bool)),
            )),
        )
    }

    scheme!(equal<av: Kind::DATA>: [Ty::Var(av), Ty::Var(av)] -> Ty::Bool);

    scheme!(compare<av: Kind::COMPARABLE>: [Ty::Var(av), Ty::Var(av)] -> Ty::Bool);

    /// `map :: ∀α β. U(α → F β) → [α] → F [β]`
    pub(crate) fn map_op(u: &mut Unifier) -> Scheme {
        let (av, bv) = (u.fresh_tyvar(), u.fresh_tyvar());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        mk_plain_scheme(
            &[av, bv],
            &[],
            thunk(fun(
                thunk(fun(a.clone(), pure(b.clone()))),
                fun(Ty::List(Box::new(a)), pure(Ty::List(Box::new(b)))),
            )),
        )
    }

    /// `filter :: ∀α. U(α → F Bool) → [α] → F [α]`
    pub(crate) fn filter_op(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        mk_plain_scheme(
            &[av],
            &[],
            thunk(fun(
                thunk(fun(a.clone(), pure(Ty::Bool))),
                fun(Ty::List(Box::new(a.clone())), pure(Ty::List(Box::new(a)))),
            )),
        )
    }

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
                    fun(Ty::List(Box::new(a)), pure(Ty::Unit)),
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
                    fun(b.clone(), fun(Ty::List(Box::new(a)), pure(b))),
                )),
            ),
        )
    }

    /// `sort-list :: ∀α:comparable. [α] → F [α]`
    pub(crate) fn sort_list(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        mk_scheme(
            &[(av, Kind::COMPARABLE)],
            &[],
            thunk(fun(
                Ty::List(Box::new(a.clone())),
                pure(Ty::List(Box::new(a))),
            )),
        )
    }

    /// `sort-list-by :: ∀α β:comparable. U(α → F β) → [α] → F [α]`
    pub(crate) fn sort_list_by(u: &mut Unifier) -> Scheme {
        let (av, bv) = (u.fresh_tyvar(), u.fresh_tyvar());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        mk_scheme(
            &[(av, Kind::ANY), (bv, Kind::COMPARABLE)],
            &[],
            thunk(fun(
                thunk(fun(a.clone(), pure(b))),
                fun(Ty::List(Box::new(a.clone())), pure(Ty::List(Box::new(a)))),
            )),
        )
    }

    // ── Strings & paths ──────────────────────────────────────────────────

    scheme!(str_to_str: [Ty::String] -> Ty::String);

    scheme!(str_to_strs: [Ty::String] -> Ty::List(Box::new(Ty::String)));

    scheme!(re_match: [Ty::String, Ty::String] -> Ty::Bool);

    scheme!(re_find_match: [Ty::String, Ty::String] -> Ty::String);

    scheme!(re_split: [Ty::String, Ty::String] -> Ty::List(Box::new(Ty::String)));

    scheme!(replace_3: [Ty::String, Ty::String, Ty::String] -> Ty::String);

    scheme!(slice: [Ty::String, Ty::Int, Ty::Int] -> Ty::String);

    /// `intercalate :: ∀α. Str → [α] → F Str`
    pub(crate) fn intercalate(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        mk_plain_scheme(
            &[av],
            &[],
            thunk(fun(
                Ty::String,
                fun(Ty::List(Box::new(Ty::Var(av))), pure(Ty::String)),
            )),
        )
    }

    // ── File system & paths ──────────────────────────────────────────────

    /// `list-dir :: Str → F [{name, type, size, mtime}]`
    pub(crate) fn list_dir(_u: &mut Unifier) -> Scheme {
        mk_plain_scheme(
            &[],
            &[],
            thunk(fun(
                Ty::String,
                pure(Ty::List(Box::new(fs_list_entry_ty()))),
            )),
        )
    }

    /// `file-info :: Str → F {…full stat…}`
    pub(crate) fn file_info(_u: &mut Unifier) -> Scheme {
        mk_plain_scheme(&[], &[], thunk(fun(Ty::String, pure(fs_file_info_ty()))))
    }

    scheme!(temp_path: pure Ty::String);

    scheme!(glob: [Ty::String] -> Ty::List(Box::new(Ty::String)));

    scheme!(is_empty<av: Kind::SIZED>: [Ty::Var(av)] -> Ty::Bool);

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
                Ty::List(Box::new(Ty::Handle(Box::new(a.clone())))),
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
        base_frame(
            &[],
            pure(closed_record(&[("pid", Ty::Int), ("desc", Ty::String)])),
        )
    }

    // ── First-class constants / queries ──────────────────────────────────

    scheme!(pure_string: pure Ty::String);

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
    scheme!(ints_to_bytes: [Ty::List(Box::new(Ty::Int))] -> writes);
    scheme!(to_any_bytes<av: Kind::DATA>: [Ty::Var(av)] -> writes);
    scheme!(to_line<av: Kind::DATA>: [Ty::Var(av)] -> writes);
    scheme!(to_lines<av: Kind::DATA>: [Ty::List(Box::new(Ty::Var(av)))] -> writes);
    scheme!(to_csv: [Ty::List(Box::new(Ty::Map(Box::new(Ty::String))))] -> writes);

    // ── Decoders ─────────────────────────────────────────────────────────
    //
    // Nullary: the bytes come from the channel, not an argument.

    scheme!(from_bytes: pure Ty::Bytes);
    scheme!(from_string: pure Ty::String);
    scheme!(from_lines: pure Ty::List(Box::new(Ty::String)));
    scheme!(from_csv: pure Ty::List(Box::new(Ty::Map(Box::new(Ty::String)))));

    /// `from-json` :: ∀α. F α — decode whatever the channel holds.
    pub fn from_json(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        mk_plain_scheme(&[av], &[], thunk(pure(Ty::Var(av))))
    }

    /// `from-json-at` :: ∀α. [String] → F α — the value at a path, whatever it
    /// holds.
    pub fn from_json_at(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let tokens = Ty::List(Box::new(Ty::String));
        mk_plain_scheme(&[av], &[], thunk(fun(tokens, pure(Ty::Var(av)))))
    }

    /// `from-jsonl` :: ∀α. F [α] — a list of whatever each line holds.
    pub fn from_jsonl(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let records = Ty::List(Box::new(Ty::Var(av)));
        mk_plain_scheme(&[av], &[], thunk(pure(records)))
    }

    // ── Range, paths, parsing ────────────────────────────────────────────

    scheme!(range: [Ty::Int, Ty::Int] -> Ty::List(Box::new(Ty::Int)));
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
pub fn builtin_type_hint(table: &BuiltinTable, name: &str) -> Option<String> {
    let mut u = Unifier::new();
    Some(fmt_scheme(&(table.get(name)?.type_rule)(&mut u)))
}

/// Detect the literal `fail [status: 0, …]` shape, so the nonzero-status rule
/// `builtins::misc::builtin_fail` enforces at runtime can be diagnosed at
/// typecheck time.
///
/// Computed statuses and spreads still defer to the runtime.
pub(crate) fn fail_status_is_zero_literal(args: &crate::ir::Args) -> bool {
    let Some(positional) = crate::ir::args::positional(args) else {
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
        let table = crate::builtins::core_builtin_table();
        for name in table.names() {
            if name == "exit" || name == "quit" {
                continue;
            }
            let entry = table.get(name).unwrap();
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
