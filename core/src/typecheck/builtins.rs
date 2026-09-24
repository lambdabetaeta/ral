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
use super::generalize::generalize;
use super::scheme::{CachedFreeVars, Scheme};
use super::ty::{CompTy, Field, Label, PayloadRoute, PayloadVar, Row, RowVar, Ty, TyVar};
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

/// Build a [`Scheme`] from its quantified vars and body.  Public so host crates
/// can write their own scheme arms without touching `Scheme`'s internals.
pub fn mk_scheme(
    ty_vars: &[TyVar],
    route_vars: &[PayloadVar],
    row_vars: &[RowVar],
    ty: Ty,
) -> Scheme {
    Scheme {
        ty_vars: ty_vars.to_vec(),
        comp_ty_vars: vec![],
        route_vars: route_vars.to_vec(),
        row_vars: row_vars.to_vec(),
        presence_vars: vec![],
        ty,
        comp_ty_bindings: vec![],
        ty_bindings: vec![],
        cached_fv: Some(CachedFreeVars::default()),
    }
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
/// An encoder's or terminal write's result: the byte channel itself is the
/// payload, so WF-2 pins the value to `Unit`.
pub(crate) fn ret_bytes() -> CompTy {
    CompTy::Return(PayloadRoute::Bytes, Box::new(Ty::Unit))
}

// ── Scheme DSL ──────────────────────────────────────────────────────
//
// `scheme!` writes a builtin's polytype declaratively: `<tv>` declares fresh
// type vars, `[...]` params curry left-to-right, `pure` means a thunked
// constant.  Each arm is labelled with the spelling it accepts.
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
    // scheme!(temp_path: pure Ty::String);
    ($name:ident: pure $ret:expr) => {
        pub fn $name(_u: &mut Unifier) -> Scheme {
            mk_scheme(&[], &[], &[], thunk(pure($ret)))
        }
    };
    // scheme!(help: bytes); — nullary, the byte channel is the payload.
    // Listed before the general `[params] -> $ret` arms below: `bytes` parses
    // as a bare expression too, so a more specific arm must come first or it
    // is never reached.
    ($name:ident: bytes) => {
        pub fn $name(_u: &mut Unifier) -> Scheme {
            mk_scheme(&[], &[], &[], thunk(ret_bytes()))
        }
    };
    // scheme!(to_bytes: [Ty::Bytes] -> bytes);
    ($name:ident: [$($p:expr),*] -> bytes) => {
        pub fn $name(_u: &mut Unifier) -> Scheme {
            mk_scheme(&[], &[], &[], thunk(curry_bytes!($($p),*)))
        }
    };
    // scheme!(str_to_str: [Ty::String] -> Ty::String);
    ($name:ident: [$($p:expr),*] -> $ret:expr) => {
        pub fn $name(_u: &mut Unifier) -> Scheme {
            mk_scheme(&[], &[], &[], thunk(curry!($($p),* => $ret)))
        }
    };
    // scheme!(to_line<av>: [Ty::Var(av)] -> bytes);
    ($name:ident<$tv:ident>: [$($p:expr),*] -> bytes) => {
        pub fn $name(u: &mut Unifier) -> Scheme {
            let $tv = u.fresh_tyvar();
            mk_scheme(&[$tv], &[], &[], thunk(curry_bytes!($($p),*)))
        }
    };
    // scheme!(length<av>: [Ty::Var(av)] -> Ty::Int);
    ($name:ident<$tv:ident>: [$($p:expr),*] -> $ret:expr) => {
        pub fn $name(u: &mut Unifier) -> Scheme {
            let $tv = u.fresh_tyvar();
            mk_scheme(&[$tv], &[], &[], thunk(curry!($($p),* => $ret)))
        }
    };
    // scheme!(compare<av,bv>: [Ty::Var(av), Ty::Var(bv)] -> Ty::Bool);
    ($name:ident<$a:ident,$b:ident>: [$($p:expr),*] -> $ret:expr) => {
        pub fn $name(u: &mut Unifier) -> Scheme {
            let ($a,$b) = (u.fresh_tyvar(), u.fresh_tyvar());
            mk_scheme(&[$a,$b], &[], &[], thunk(curry!($($p),* => $ret)))
        }
    };
}

/// Right-fold parameters into `fun(p₁, fun(p₂, …, pure(ret)))`.
macro_rules! curry {
    ($p:expr => $ret:expr) => { fun($p, pure($ret)) };
    ($p:expr, $($rest:expr),+ => $ret:expr) => { fun($p, curry!($($rest),+ => $ret)) };
}

/// [`curry`]'s encoder dual: the fold's base case is [`ret_bytes`], not a
/// named return type.
macro_rules! curry_bytes {
    ($p:expr) => { fun($p, ret_bytes()) };
    ($p:expr, $($rest:expr),+) => { fun($p, curry_bytes!($($rest),+)) };
}

/// A record type over a row of fields ending in `tail`.
pub fn record_row(fields: &[(&str, Ty)], tail: Row) -> Ty {
    let mut row = tail;
    for (l, t) in fields.iter().rev() {
        row = Row::Extend(
            Label::Field((*l).to_string()),
            Field::present(t.clone()),
            Box::new(row),
        );
    }
    Ty::Record(row)
}

/// A record type over a closed row: the tail is `Empty`, so no extension.
pub fn closed_record(fields: &[(&str, Ty)]) -> Ty {
    record_row(fields, Row::Empty)
}

/// A record type left open on `tail`: the one shape a row can give an
/// *optional* field, whose type is then the reader's to check.
pub fn open_record(fields: &[(&str, Ty)], tail: RowVar) -> Ty {
    record_row(fields, Row::Var(tail))
}

/// A variant type over a row of tags with stated payloads, ending in `tail`.
pub fn variant_row(tags: &[(&str, Ty)], tail: Row) -> Ty {
    let mut row = tail;
    for (l, t) in tags.iter().rev() {
        row = Row::Extend(
            Label::Case((*l).to_string()),
            Field::present(t.clone()),
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
        Field::present(Ty::Int),
        Box::new(Row::Extend(
            Label::Field("message".into()),
            Field::present(Ty::String),
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
        CompTy, PayloadRoute, PayloadVar, Row, Scheme, Ty, TyEnv, TyVar, Unifier, await_record,
        error_record_shape, fs_file_info_ty, fs_list_entry_ty, fun, generalize, lines_step_ty,
        mk_scheme, poll_variant, pure, ret_bytes, thunk,
    };

    // ── List operations ──────────────────────────────────────────────────

    scheme!(length<av>: [Ty::Var(av)] -> Ty::Int);

    /// `surface :: ∀ρ. Variant ρ → F ()` — forward a tagged event to the host's
    /// event sink.  The row stays open: the host decides which tags it knows.
    ///
    /// `pub`: a host declares its own [`BuiltinEntry`](crate::types::BuiltinEntry)
    /// over this scheme under its own name; see
    /// [`crate::builtins::SURFACE_BUILTIN`].
    pub fn surface_op(u: &mut Unifier) -> Scheme {
        let row = u.fresh_row_var();
        mk_scheme(
            &[],
            &[],
            &[row],
            thunk(fun(Ty::Variant(Row::Var(row)), pure(Ty::Unit))),
        )
    }

    /// `keys :: ∀α. Map<α> → F [Str]`
    pub fn keys(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        mk_scheme(
            &[av],
            &[],
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
        mk_scheme(
            &[av],
            &[],
            &[],
            thunk(fun(
                Ty::Map(Box::new(Ty::Var(av))),
                fun(Ty::String, pure(Ty::Bool)),
            )),
        )
    }

    scheme!(compare<av,bv>: [Ty::Var(av), Ty::Var(bv)] -> Ty::Bool);

    /// Result type of a higher-order callback: `F[ρ] τ`, its route
    /// scheme-quantified — `map { echo $x }` needs it free to instantiate
    /// `Bytes`.
    fn callback_result(u: &mut Unifier, ty: Ty) -> (PayloadVar, CompTy) {
        let rv = u.fresh_routevar();
        (rv, CompTy::Return(PayloadRoute::Var(rv), Box::new(ty)))
    }

    /// `map :: ∀α β ρ. U(α → F[ρ] β) → [α] → F [β]`
    pub(crate) fn map_op(u: &mut Unifier) -> Scheme {
        let (av, bv) = (u.fresh_tyvar(), u.fresh_tyvar());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        let (rv, cb_result) = callback_result(u, b.clone());
        mk_scheme(
            &[av, bv],
            &[rv],
            &[],
            thunk(fun(
                thunk(fun(a.clone(), cb_result)),
                fun(Ty::List(Box::new(a)), pure(Ty::List(Box::new(b)))),
            )),
        )
    }

    /// `filter :: ∀α ρ. U(α → F[ρ] Bool) → [α] → F [α]`
    pub(crate) fn filter_op(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        let (rv, cb_result) = callback_result(u, Ty::Bool);
        mk_scheme(
            &[av],
            &[rv],
            &[],
            thunk(fun(
                thunk(fun(a.clone(), cb_result)),
                fun(Ty::List(Box::new(a.clone())), pure(Ty::List(Box::new(a)))),
            )),
        )
    }

    /// `each :: ∀α β ρ. U(α → F[ρ] β) → [α] → F Unit`
    pub(crate) fn each_op(u: &mut Unifier) -> Scheme {
        let (av, bv) = (u.fresh_tyvar(), u.fresh_tyvar());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        let (rv, cb_result) = callback_result(u, b);
        mk_scheme(
            &[av, bv],
            &[rv],
            &[],
            thunk(fun(
                thunk(fun(a.clone(), cb_result)),
                fun(Ty::List(Box::new(a)), pure(Ty::Unit)),
            )),
        )
    }

    /// `fold :: ∀α β ρ. U(β → α → F[ρ] β) → β → [α] → F β`
    pub(crate) fn fold_op(u: &mut Unifier) -> Scheme {
        let (av, bv) = (u.fresh_tyvar(), u.fresh_tyvar());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        let (rv, cb_result) = callback_result(u, b.clone());
        mk_scheme(
            &[av, bv],
            &[rv],
            &[],
            thunk(fun(
                thunk(fun(b.clone(), fun(a.clone(), cb_result))),
                fun(b.clone(), fun(Ty::List(Box::new(a)), pure(b))),
            )),
        )
    }

    /// `sort-list :: ∀α. [α] → F [α]`
    pub(crate) fn sort_list(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        mk_scheme(
            &[av],
            &[],
            &[],
            thunk(fun(
                Ty::List(Box::new(a.clone())),
                pure(Ty::List(Box::new(a))),
            )),
        )
    }

    /// `sort-list-by :: ∀α β ρ. U(α → F[ρ] β) → [α] → F [α]`
    pub(crate) fn sort_list_by(u: &mut Unifier) -> Scheme {
        let (av, bv) = (u.fresh_tyvar(), u.fresh_tyvar());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        let (rv, cb_result) = callback_result(u, b);
        mk_scheme(
            &[av, bv],
            &[rv],
            &[],
            thunk(fun(
                thunk(fun(a.clone(), cb_result)),
                fun(Ty::List(Box::new(a.clone())), pure(Ty::List(Box::new(a)))),
            )),
        )
    }

    // ── Strings & paths ──────────────────────────────────────────────────

    scheme!(str_to_str: [Ty::String] -> Ty::String);

    scheme!(shell_split: [Ty::String] -> Ty::List(Box::new(Ty::String)));

    scheme!(re_match: [Ty::String, Ty::String] -> Ty::Bool);

    scheme!(re_find_match: [Ty::String, Ty::String] -> Ty::String);

    scheme!(re_split: [Ty::String, Ty::String] -> Ty::List(Box::new(Ty::String)));

    scheme!(replace_3: [Ty::String, Ty::String, Ty::String] -> Ty::String);

    scheme!(slice: [Ty::String, Ty::Int, Ty::Int] -> Ty::String);

    /// `intercalate :: ∀α. Str → [α] → F Str`
    pub(crate) fn intercalate(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        mk_scheme(
            &[av],
            &[],
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
        mk_scheme(
            &[],
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
        mk_scheme(
            &[],
            &[],
            &[],
            thunk(fun(Ty::String, pure(fs_file_info_ty()))),
        )
    }

    scheme!(temp_path: pure Ty::String);

    scheme!(glob: [Ty::String] -> Ty::List(Box::new(Ty::String)));

    scheme!(is_empty<av>: [Ty::Var(av)] -> Ty::Bool);

    // ── Streaming reducers ───────────────────────────────────────────────

    /// `fold-lines :: ∀α ρ. U(α → Str → F[ρ] α) → α → F[ρ] α`
    ///
    /// The callback's route and the reducer's own route are one variable: a
    /// callback whose *tail* is a byte write — `map-lines` and `filter-lines`
    /// in `prelude.ral` — makes the whole stage a byte producer feeding
    /// downstream, while `return $acc` keeps the route `Value` and the
    /// accumulator comes home as a value.  WF-2 survives the forwarding: at
    /// `ρ = Bytes` the callback's own value is `Unit`, and `α` is what the
    /// reducer returns.
    pub(crate) fn fold_lines(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        let rv = u.fresh_routevar();
        let route = PayloadRoute::Var(rv);
        mk_scheme(
            &[av],
            &[rv],
            &[],
            thunk(fun(
                thunk(fun(
                    a.clone(),
                    fun(Ty::String, CompTy::Return(route, Box::new(a.clone()))),
                )),
                fun(a.clone(), CompTy::Return(route, Box::new(a))),
            )),
        )
    }

    // ── Concurrency ──────────────────────────────────────────────────────

    /// `spawn :: ∀α ρ. U(F[ρ] α) → F (Handle α)`
    pub fn spawn(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        let rv = u.fresh_routevar();
        let body = CompTy::Return(PayloadRoute::Var(rv), Box::new(a.clone()));
        mk_scheme(
            &[av],
            &[rv],
            &[],
            thunk(fun(thunk(body), pure(Ty::Handle(Box::new(a))))),
        )
    }

    /// `watch :: ∀α ρ. String → U(F[ρ] α) → F (Handle α)`
    pub(crate) fn watch(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        let rv = u.fresh_routevar();
        let body = CompTy::Return(PayloadRoute::Var(rv), Box::new(a.clone()));
        mk_scheme(
            &[av],
            &[rv],
            &[],
            thunk(fun(
                Ty::String,
                fun(thunk(body), pure(Ty::Handle(Box::new(a)))),
            )),
        )
    }

    /// `service :: ∀α ρ. String → U(F[ρ] α) → F (Handle α)` — `watch`'s
    /// scheme, the leading `String` being the mandatory birth description.
    ///
    /// The durable lease class is a runtime fact, invisible to the types.
    pub(crate) fn service(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        let rv = u.fresh_routevar();
        let body = CompTy::Return(PayloadRoute::Var(rv), Box::new(a.clone()));
        mk_scheme(
            &[av],
            &[rv],
            &[],
            thunk(fun(
                Ty::String,
                fun(thunk(body), pure(Ty::Handle(Box::new(a)))),
            )),
        )
    }

    /// `await :: ∀α. Handle α → F {value, stdout, stderr}`
    pub(crate) fn await_op(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        mk_scheme(
            &[av],
            &[],
            &[],
            thunk(fun(Ty::Handle(Box::new(a.clone())), pure(await_record(a)))),
        )
    }

    /// `poll :: ∀α. Handle α → F <pending: {stdout, stderr} | settled: {stdout, stderr, outcome: <ok: α | err: ErrRecord>}>`
    pub(crate) fn poll(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        mk_scheme(
            &[av],
            &[],
            &[],
            thunk(fun(Ty::Handle(Box::new(a.clone())), pure(poll_variant(a)))),
        )
    }

    /// `race :: ∀α. [Handle α] → F {value, stdout, stderr}`
    pub(crate) fn race(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        mk_scheme(
            &[av],
            &[],
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
        mk_scheme(
            &[av],
            &[],
            &[],
            thunk(fun(Ty::Handle(Box::new(Ty::Var(av))), pure(Ty::Unit))),
        )
    }

    // ── Base frames ──────────────────────────────────────────────────────

    /// A base frame's type: an argv in, the computation the frame names out.
    /// One shape for the whole argv half of the manifest — a frame differs
    /// from its siblings only in the result it names.
    fn base_frame(ty_vars: &[TyVar], result: CompTy) -> Scheme {
        mk_scheme(ty_vars, &[], &[], thunk(fun(Ty::argv(), result)))
    }

    /// `echo :: [Str] → F[Bytes] ()` — join the argv with single spaces and
    /// write it with a trailing newline.  Byte-routed, so pipeline typing
    /// reads it as the write it is.
    pub(crate) fn echo(_u: &mut Unifier) -> Scheme {
        base_frame(&[], CompTy::bytes())
    }

    /// `detach :: ∀α. [Str] → F α`.
    ///
    /// The `{pid, desc}` receipt is a record, and one frame does not earn a
    /// former for it, so the caller reads it as whatever it needs; the runtime
    /// hands back the whole record either way.
    pub fn detach(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        base_frame(&[av], pure(Ty::Var(av)))
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
        mk_scheme(
            &[],
            &[],
            &[rv],
            thunk(fun(Ty::String, pure(Ty::Record(Row::Var(rv))))),
        )
    }

    // ── Terminal, help & encoders ────────────────────────────────────────
    //
    // Each writes to the byte channel: `F[Bytes] ()`, WF-2's `value = Unit`
    // pinned by [`ret_bytes`].

    scheme!(terminal_control: bytes);
    scheme!(help: bytes);
    scheme!(explain: [Ty::String] -> bytes);
    scheme!(to_bytes: [Ty::Bytes] -> bytes);
    scheme!(ints_to_bytes: [Ty::List(Box::new(Ty::Int))] -> bytes);
    scheme!(to_any_bytes<av>: [Ty::Var(av)] -> bytes);
    scheme!(to_line<av>: [Ty::Var(av)] -> bytes);
    scheme!(to_lines<av>: [Ty::List(Box::new(Ty::Var(av)))] -> bytes);

    // ── Decoders ─────────────────────────────────────────────────────────
    //
    // Nullary: the bytes come from the channel, not an argument.

    scheme!(from_bytes: pure Ty::Bytes);
    scheme!(from_string: pure Ty::String);

    /// `from-json`/`from-csv` :: ∀α. F α — decode whatever the channel holds.
    pub fn from_json(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        mk_scheme(&[av], &[], &[], thunk(pure(Ty::Var(av))))
    }

    /// `from-lines` :: F (Step of lines).
    ///
    /// The recursion closes through a comp var, which [`mk_scheme`] cannot
    /// quantify (its `comp_ty_bindings` is always empty), so this goes
    /// through [`generalize`] instead, exactly as the templates it replaces
    /// did.
    pub(crate) fn from_lines(u: &mut Unifier) -> Scheme {
        let step = lines_step_ty(u);
        generalize(u, &TyEnv::new(), &thunk(pure(step)))
    }

    // ── Range, paths, parsing ────────────────────────────────────────────

    scheme!(range: [Ty::Int, Ty::Int] -> Ty::List(Box::new(Ty::Int)));
    scheme!(chdir: [Ty::String] -> Ty::Unit);
    scheme!(path_bool: [Ty::String] -> Ty::Bool);
    scheme!(int_parse<av>: [Ty::Var(av)] -> Ty::Int);
    scheme!(float_parse<av>: [Ty::Var(av)] -> Ty::Float);
    scheme!(str_parse<av>: [Ty::Var(av)] -> Ty::String);
    scheme!(round: [Ty::Float, Ty::Int] -> Ty::Float);
    // Shared by `floor`, `ceil`, and `trunc`.
    scheme!(float_to_int: [Ty::Float] -> Ty::Int);

    // ── Aliasing, job control, plugins ───────────────────────────────────

    /// `alias :: String → U(comp) → F Unit`.
    ///
    /// The block's own computation type is unconstrained here (only its
    /// arity is checked elsewhere), so the comp var it mints must be
    /// quantified through [`generalize`], not [`mk_scheme`], for the same
    /// reason as [`from_lines`].
    pub(crate) fn alias(u: &mut Unifier) -> Scheme {
        let block = u.fresh_comp_ty();
        let body = fun(Ty::String, fun(thunk(block), pure(Ty::Unit)));
        generalize(u, &TyEnv::new(), &thunk(body))
    }
    scheme!(unalias: [Ty::String] -> Ty::Unit);

    scheme!(string_to_unit: [Ty::String] -> Ty::Unit);

    // ── Divergence ───────────────────────────────────────────────────────

    /// `fail :: ∀α ρ r. {status: Int, message: String | r} → F[ρ] α`.
    ///
    /// An error record, open at the tail so a caught error re-raises with the
    /// fields `try` gave it.  Divergent, so its route and value join whatever
    /// the context needs rather than forcing `Unit` on the other arm of an
    /// `if`.
    pub(crate) fn fail(u: &mut Unifier) -> Scheme {
        let row = u.fresh_row_var();
        let av = u.fresh_tyvar();
        let rv = u.fresh_routevar();
        mk_scheme(
            &[av],
            &[rv],
            &[row],
            thunk(fun(
                error_record_shape(row),
                CompTy::Return(PayloadRoute::Var(rv), Box::new(Ty::Var(av))),
            )),
        )
    }

    /// `exit`/`quit` :: ∀α ρ. Int → F[ρ] α — a status, and no return.
    /// Divergent like [`fail`]; the elaborator sugars bare `exit` to `exit 0`.
    pub(crate) fn exit(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let rv = u.fresh_routevar();
        mk_scheme(
            &[av],
            &[rv],
            &[],
            thunk(fun(
                Ty::Int,
                CompTy::Return(PayloadRoute::Var(rv), Box::new(Ty::Var(av))),
            )),
        )
    }

    /// `∀α ρ. F[ρ] α` — nullary and divergent like [`fail`]/[`exit`].
    ///
    /// For a host builtin that never returns (a test-only Rust panic trigger,
    /// say): its route and value join whatever the context needs rather than
    /// forcing one on it.
    pub fn diverges(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let rv = u.fresh_routevar();
        mk_scheme(
            &[av],
            &[rv],
            &[],
            thunk(CompTy::Return(PayloadRoute::Var(rv), Box::new(Ty::Var(av)))),
        )
    }
}

/// A value builtin's first-class polytype: the type a `$name` reference holds.
///
/// `None` when `table` has no value row under `name`.  A base frame is absent
/// by construction — it takes an argv, and no value does — so this is also the
/// question "may `$name` hold it?".
///
/// Resolution runs against `table`, the checked session's own surface, so a
/// name means what that session evaluates.
pub(crate) fn builtin_scheme(table: &BuiltinTable, name: &str, u: &mut Unifier) -> Option<Scheme> {
    Some((table.value(name)?.type_rule)(u))
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

/// The value `from-lines` returns, standalone from an [`Inferencer`] context:
/// a recursive Step stream of Strings, the recursion closing through a comp
/// var, not a `TyVar`.
///
/// # Panics
///
/// Never: the self-referential unification it performs is between a fresh
/// comp var and the type built from it, which cannot fail.
pub(in crate::typecheck) fn lines_step_ty(u: &mut Unifier) -> Ty {
    use crate::stream::{DONE_LABEL, HEAD_FIELD, MORE_LABEL, TAIL_FIELD};
    let tail_comp = u.fresh_comp_ty();
    let payload = Ty::Record(Row::Extend(
        Label::Field(HEAD_FIELD.into()),
        Field::present(Ty::String),
        Box::new(Row::Extend(
            Label::Field(TAIL_FIELD.into()),
            Field::present(Ty::Thunk(Box::new(tail_comp.clone()))),
            Box::new(Row::Empty),
        )),
    ));
    let step = Ty::Variant(Row::Extend(
        Label::Case(MORE_LABEL.into()),
        Field::present(payload),
        Box::new(Row::Extend(
            Label::Case(DONE_LABEL.into()),
            Field::present(Ty::Unit),
            Box::new(Row::Empty),
        )),
    ));
    u.unify_comp_ty(&tail_comp, &CompTy::pure(step.clone()))
        .expect("fresh self-referential unify cannot fail");
    step
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
        Some(crate::ir::Val::Record(entries)) if entries.iter().any(|e| matches!(
            e,
            crate::ir::ValRecordEntry::Field(
                k,
                crate::source::Spanned {
                    item: crate::ir::Val::Int(0),
                    ..
                },
            ) if k == "status"
        ))
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
