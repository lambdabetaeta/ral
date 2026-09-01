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
use super::error::TypeErrorKind;
use super::fmt::fmt_scheme;
use super::generalize::generalize;
use super::infer::Inferencer;
use super::scheme::{CachedFreeVars, Scheme};
use super::ty::{CompTy, PayloadRoute, PayloadVar, Row, RowVar, Ty, TyVar};
use super::unify::Unifier;
use crate::types::BuiltinTable;

/// How the type checker types a registered builtin: a scheme factory, run
/// fresh against a live [`Unifier`].
///
/// A base frame names one directly, the same as any value builtin — an argv
/// is one argument of one type, not a list of slots to diagnose one by one.
pub type BuiltinTypeRule = fn(&mut Unifier) -> Scheme;

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
pub enum BuiltinDiagnostic {
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
pub fn ret_bytes() -> CompTy {
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

/// A record type over a closed row: the tail is `Empty`, so no extension.
pub fn closed_record(fields: &[(&str, Ty)]) -> Ty {
    let mut row = Row::Empty;
    for (l, t) in fields.iter().rev() {
        row = Row::Extend(l.to_string(), Box::new(t.clone()), Box::new(row));
    }
    Ty::Record(row)
}

/// The error record a raising form demands of its argument: `status`, over a
/// fresh tail.  Open, because re-raising a caught error carries `cmd`, `line`,
/// `col` and whatever else the record picked up along the way; the tail is
/// what makes [`try_error_record`] an instance of this shape.
///
/// `message` is absent by design: it is optional, and its type is `String` or
/// `Bytes` — a union no row can spell.  [`Inferencer::check_error_message`]
/// judges it once the row has been unified.  The caller quantifies the row
/// itself — [`scheme::fail`] — so this takes it directly rather than minting
/// one.
pub(in crate::typecheck) fn error_record_shape(row: RowVar) -> Ty {
    Ty::Record(Row::Extend(
        "status".into(),
        Box::new(Ty::Int),
        Box::new(Row::Var(row)),
    ))
}

/// The error record `try` hands its handler, mirrored at runtime by
/// `error_record` in `core/src/evaluator/scope.rs`.  `message` is synthetic
/// status text, never the failing command's fd 2 bytes: those streamed live,
/// and `audit` is the forensic path.
pub(super) fn try_error_record() -> Ty {
    closed_record(&[
        ("status", Ty::Int),
        ("cmd", Ty::String),
        ("message", Ty::String),
        ("line", Ty::Int),
        ("col", Ty::Int),
    ])
}

/// The record `audit { … }` produces, field for field the value shape
/// `evaluator::audit::tree_value` materialises.  A child in `children` is a
/// fresh map type, one of the shapes `Observation::to_value` projects, so an
/// observation's own fields can grow without breaking this.
pub(super) fn audit_record(value_ty: Ty, child_ty: Ty) -> Ty {
    closed_record(&[
        ("status", Ty::Int),
        ("value", value_ty),
        ("error", Ty::String),
        ("children", Ty::List(Box::new(Ty::Map(Box::new(child_ty))))),
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
    use crate::syntax::tag::tag_row_label;
    let outcome = Ty::Variant(Row::Extend(
        tag_row_label("ok"),
        Box::new(value_ty),
        Box::new(Row::Extend(
            tag_row_label("err"),
            Box::new(try_error_record()),
            Box::new(Row::Empty),
        )),
    ));
    closed_record(&[
        ("stdout", Ty::Bytes),
        ("stderr", Ty::Bytes),
        ("outcome", outcome),
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
    use crate::syntax::tag::tag_row_label;
    Ty::Variant(Row::Extend(
        tag_row_label("pending"),
        Box::new(pending_record()),
        Box::new(Row::Extend(
            tag_row_label("settled"),
            Box::new(settle_record(value_ty)),
            Box::new(Row::Empty),
        )),
    ))
}

/// The record type returned by `list-dir` for each directory entry.
pub fn fs_list_entry_ty() -> Ty {
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
pub fn fs_file_info_ty() -> Ty {
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
    pub fn has(u: &mut Unifier) -> Scheme {
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
    pub fn map_op(u: &mut Unifier) -> Scheme {
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
    pub fn filter_op(u: &mut Unifier) -> Scheme {
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
    pub fn each_op(u: &mut Unifier) -> Scheme {
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
    pub fn fold_op(u: &mut Unifier) -> Scheme {
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
    pub fn sort_list(u: &mut Unifier) -> Scheme {
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
    pub fn sort_list_by(u: &mut Unifier) -> Scheme {
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
    pub fn intercalate(u: &mut Unifier) -> Scheme {
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
    pub fn list_dir(_u: &mut Unifier) -> Scheme {
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
    pub fn file_info(_u: &mut Unifier) -> Scheme {
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
    pub fn fold_lines(u: &mut Unifier) -> Scheme {
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
    pub fn watch(u: &mut Unifier) -> Scheme {
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
    pub fn service(u: &mut Unifier) -> Scheme {
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
    pub fn await_op(u: &mut Unifier) -> Scheme {
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
    pub fn poll(u: &mut Unifier) -> Scheme {
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
    pub fn race(u: &mut Unifier) -> Scheme {
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
    pub fn cancel_op(u: &mut Unifier) -> Scheme {
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
    pub fn echo(_u: &mut Unifier) -> Scheme {
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

    scheme!(source_op<av>: [Ty::String] -> Ty::Var(av));

    scheme!(use_op<av>: [Ty::String] -> Ty::Map(Box::new(Ty::Var(av))));

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
    pub fn from_lines(u: &mut Unifier) -> Scheme {
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
    pub fn alias(u: &mut Unifier) -> Scheme {
        let block = u.fresh_comp_ty();
        let body = fun(Ty::String, fun(thunk(block), pure(Ty::Unit)));
        generalize(u, &TyEnv::new(), &thunk(body))
    }
    scheme!(unalias: [Ty::String] -> Ty::Unit);

    // Shared by `fg`/`bg`/`disown`, registered by the REPL host in
    // `ral/src/repl/host_handlers.rs`: the job to act on, named.
    scheme!(int_to_unit: [Ty::Int] -> Ty::Unit);
    scheme!(string_to_unit: [Ty::String] -> Ty::Unit);

    // ── Divergence ───────────────────────────────────────────────────────

    /// `fail :: ∀α ρ r. {status: Int | r} → F[ρ] α`.
    ///
    /// An error record, open at the tail so a caught error re-raises with the
    /// fields `try` gave it.  Divergent, so its route and value join whatever
    /// the context needs rather than forcing `Unit` on the other arm of an
    /// `if`.
    pub fn fail(u: &mut Unifier) -> Scheme {
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
    pub fn exit(u: &mut Unifier) -> Scheme {
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
}

/// A value builtin's first-class polytype: the type a `$name` reference holds.
///
/// `None` when `table` has no value row under `name`.  A base frame is absent
/// by construction — it takes an argv, and no value does — so this is also the
/// question "may `$name` hold it?".
///
/// Resolution runs against `table`, the checked session's own surface, so a
/// name means what that session evaluates.
pub fn builtin_scheme(table: &BuiltinTable, name: &str, u: &mut Unifier) -> Option<Scheme> {
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
    use crate::stream::{HEAD_FIELD, TAIL_FIELD, done_tag, more_tag};
    let tail_comp = u.fresh_comp_ty();
    let payload = Ty::Record(Row::Extend(
        HEAD_FIELD.into(),
        Box::new(Ty::String),
        Box::new(Row::Extend(
            TAIL_FIELD.into(),
            Box::new(Ty::Thunk(Box::new(tail_comp.clone()))),
            Box::new(Row::Empty),
        )),
    ));
    let step = Ty::Variant(Row::Extend(
        more_tag(),
        Box::new(payload),
        Box::new(Row::Extend(
            done_tag(),
            Box::new(Ty::Unit),
            Box::new(Row::Empty),
        )),
    ));
    u.unify_comp_ty(&tail_comp, &CompTy::pure(step.clone()))
        .expect("fresh self-referential unify cannot fail");
    step
}

/// A per-key type schema, driving `check_map_entry_fields` in `super::infer`.
/// `None` for a key leaves that entry runtime-dispatched: still inferred for its
/// side-effects, but unified against nothing.
pub type FieldSchema = fn(&str, &mut Unifier) -> Option<Ty>;

/// Schema for rc plugin entries `[plugin: Str, options: Map]`.
pub fn plugin_entry_field_ty(key: &str, u: &mut Unifier) -> Option<Ty> {
    match key {
        "plugin" => Some(Ty::String),
        "options" => Some(Ty::Map(Box::new(u.fresh_ty()))),
        _ => None,
    }
}

/// Detect the literal `fail [status: 0, …]` shape, so the nonzero-status rule
/// `builtins::misc::builtin_fail` enforces at runtime can be diagnosed at
/// typecheck time.
///
/// Computed statuses and spreads still defer to the runtime.
pub fn fail_status_is_zero_literal(args: &crate::ir::Args) -> bool {
    let Some(positional) = crate::ir::args::positional(args) else {
        return false;
    };
    matches!(
        positional.first(),
        Some(crate::ir::Val::Map(entries)) if entries.iter().any(|e| matches!(
            e,
            crate::ir::ValMapEntry::Entry(
                crate::ir::Val::String(k),
                crate::source::Spanned {
                    item: crate::ir::Val::Int(0),
                    ..
                },
            ) if k == "status"
        ))
    )
}

impl Inferencer<'_> {
    /// The half of the error-record shape the row cannot carry: a `message`,
    /// if the record has one, is `String` or `Bytes`.  Read after unification,
    /// so a field arriving through the open tail is judged too.  A field still
    /// a variable stays free — the runtime takes either spelling, so pinning
    /// one here would be a guess.
    pub(super) fn check_error_message(&mut self, actual: &Ty) {
        let Ty::Record(row) = self.ctx.unifier.apply_ty(actual) else {
            return;
        };
        let mut rest = row;
        let message = loop {
            match rest {
                Row::Extend(label, ty, tail) => {
                    if label == "message" {
                        break *ty;
                    }
                    rest = *tail;
                }
                Row::Empty | Row::Var(_) => return,
            }
        };
        if !matches!(message, Ty::String | Ty::Bytes | Ty::Var(_)) {
            self.ctx
                .diagnose(TypeErrorKind::ErrorRecordMessage { actual: message });
        }
    }
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
