//! Type-checker side of the builtin registry: per-entry typing
//! rules, the scheme factories they pick from, and shared helpers
//! for record-shaped builtins (audit, try-error, await, fs).
//!
//! Each registry entry in `core/src/builtins.rs` carries a `ty:`
//! field that selects either a first-class [`Scheme`] factory or an
//! explicit command [`BuiltinSig`].  Scheme factories allocate fresh
//! unifier vars directly, so the returned scheme can be stored in the
//! env or used at a call site without any post-processing renaming
//! step.  Command signatures are interpreted by the inferencer as
//! data: argument policy, computation result, and optional diagnostic
//! probe.
//!
//! [`Scheme`]: BuiltinTypeRule::Scheme

use super::error::{Reason, TypeErrorKind};
use super::fmt::{fmt_scheme, fmt_ty};
use super::infer::Inferencer;
use super::scheme::{CachedFreeVars, Scheme};
use super::ty::{CompTy, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};
use super::unify::Unifier;
use crate::types::BuiltinTable;

/// How the type checker handles a call to a registered builtin.
///
/// `Scheme` entries are ordinary first-class bindings: the scheme is
/// applied in command position and can be reified in value position.
/// `Sig` entries are command signatures: they describe argv shape and
/// the resulting computation directly, without falling through to
/// command-name classification or running per-builtin inference code.
#[derive(Clone, Copy)]
pub enum BuiltinTypeRule {
    /// Standard polytype.  The function allocates fresh unifier vars
    /// each call. `arity` caches the number of value arguments for
    /// `$name` synthesis — the registry entry's declared `arity:` field,
    /// cross-checked against the scheme's `Fun` nesting only by a
    /// debug assertion; `None` for variadic / command-only entries.
    Scheme(Option<usize>, fn(&mut Unifier) -> Scheme),
    /// Command-position signature.  `value` decides whether `$name`
    /// has a first-class form; `None` means command-only.
    Sig(BuiltinSig),
}

/// Data signature for a builtin that is typed as a command operation.
#[derive(Clone, Copy)]
pub struct BuiltinSig {
    pub args: ArgSig,
    pub result: CompTemplate,
    pub value: Option<fn(&mut Unifier) -> Scheme>,
    pub diagnostic: BuiltinDiagnostic,
}

impl BuiltinSig {
    /// Fixed value-arg count implied by the argument signature;
    /// `None` for variadic / optional / open argument policies.
    pub const fn fixed_arity(&self) -> Option<usize> {
        match self.args {
            ArgSig::Exact(t) | ArgSig::DataLast(t) => Some(t.len()),
            _ => None,
        }
    }
}

/// Argument policy for a builtin command signature.
#[derive(Clone, Copy)]
pub enum ArgSig {
    Exact(&'static [ArgTemplate]),
    DataLast(&'static [ArgTemplate]),
    Optional(ArgTemplate),
    Any,
}

/// One argument slot in a command signature.
#[derive(Clone, Copy)]
pub enum ArgTemplate {
    Ty(TyTemplate),
    Any,
    BlockOrLambda,
    OneOf(&'static [Self]),
}

/// Small type-template vocabulary used by command signatures.
#[derive(Clone, Copy)]
pub enum TyTemplate {
    String,
    Int,
    Float,
    Bool,
    Bytes,
    Unit,
    Any,
    ListAny,
    ListInt,
}

/// Computation template returned by a command signature.
#[derive(Clone, Copy)]
pub enum CompTemplate {
    Pure(TyTemplate),
    Return {
        input: ModeTemplate,
        output: ModeTemplate,
        value: TyTemplate,
    },
    Never,
    LinesStep,
}

/// Pipe-mode template used by [`CompTemplate`].
#[derive(Clone, Copy)]
pub enum ModeTemplate {
    None,
    Bytes,
    Fresh,
}

/// Extra non-typing behaviour attached to a builtin signature.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BuiltinDiagnostic {
    None,
    FailStatusNonzero,
    TypeProbe,
}

/// Project a [`ModeTemplate`] onto a concrete [`PipeMode`], minting a
/// fresh variable for `Fresh` from the checker's unifier.
pub fn mode_of_template(template: ModeTemplate, u: &mut Unifier) -> PipeMode {
    match template {
        ModeTemplate::None => PipeMode::None,
        ModeTemplate::Bytes => PipeMode::Bytes,
        ModeTemplate::Fresh => u.fresh_mode(),
    }
}

/// The boundary [`PipeSpec`] of a command signature: the modal projection
/// of its result template — the single source of a `Sig` builtin's modes,
/// from which the checker builds its `CompTy`.
pub fn sig_pipe_spec(result: &CompTemplate, u: &mut Unifier) -> PipeSpec {
    match result {
        CompTemplate::Pure(_) => PipeSpec::none(),
        CompTemplate::Return { input, output, .. } => PipeSpec {
            input: mode_of_template(*input, u),
            output: mode_of_template(*output, u),
        },
        CompTemplate::Never => PipeSpec {
            input: u.fresh_mode(),
            output: u.fresh_mode(),
        },
        CompTemplate::LinesStep => PipeSpec::decode(),
    }
}

/// The boundary [`PipeSpec`] of a streaming reducer (`fold-lines`): bytes
/// in, output following the callback's output mode.
///
/// The checker supplies
/// `callback_output` from the callback's quantified mode variable.
pub fn reducer_spec(callback_output: PipeMode) -> PipeSpec {
    PipeSpec {
        input: PipeMode::Bytes,
        output: callback_output,
    }
}

/// Construct a [`Scheme`] from its quantified vars and body.  Exposed
/// so host crates can build their scheme arms without depending on
/// `Scheme`'s private internals.
pub fn mk_scheme(ty_vars: &[TyVar], mode_vars: &[ModeVar], row_vars: &[RowVar], ty: Ty) -> Scheme {
    Scheme {
        ty_vars: ty_vars.to_vec(),
        comp_ty_vars: vec![],
        mode_vars: mode_vars.to_vec(),
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

// ── Scheme DSL ──────────────────────────────────────────────────────
//
// The [`scheme!`] macro writes a builtin's type scheme in a compact
// declarative form, e.g.:
//
//   scheme!(str_to_str: [Ty::String] -> Ty::String);
//   scheme!(length<av>: [Ty::Var(av)] -> Ty::Int);
//   scheme!(compare<av,bv>: [Ty::Var(av), Ty::Var(bv)] -> Ty::Bool);
//   scheme!(temp_path: pure Ty::String);
//   scheme!(ask: pipe [Ty::String] -> Ty::String);
//
// Parameters in `[...]` are curried left-to-right; `->` separates the
// last parameter from the return type.  `<tv>` declares fresh unifier
// type variables.  `pure` denotes a thunked constant; `pipe` generates
// fresh pipe-mode variables via [`fm`].

macro_rules! scheme {
    // Pure value: scheme!(temp_path: pure Ty::String);
    ($name:ident: pure $ret:expr) => {
        pub fn $name(_u: &mut Unifier) -> Scheme {
            mk_scheme(&[], &[], &[], thunk(pure($ret)))
        }
    };
    // N params, 0 type vars: scheme!(str_to_str: [Ty::String] -> Ty::String);
    ($name:ident: [$($p:expr),*] -> $ret:expr) => {
        pub fn $name(_u: &mut Unifier) -> Scheme {
            mk_scheme(&[], &[], &[], thunk(curry!($($p),* => $ret)))
        }
    };
    // N params, 1 type var: scheme!(length<av>: [Ty::Var(av)] -> Ty::Int);
    ($name:ident<$tv:ident>: [$($p:expr),*] -> $ret:expr) => {
        pub fn $name(u: &mut Unifier) -> Scheme {
            let $tv = u.fresh_tyvar();
            mk_scheme(&[$tv], &[], &[], thunk(curry!($($p),* => $ret)))
        }
    };
    // N params, 2 type vars: scheme!(compare<av,bv>: [Ty::Var(av), Ty::Var(bv)] -> Ty::Bool);
    ($name:ident<$a:ident,$b:ident>: [$($p:expr),*] -> $ret:expr) => {
        pub fn $name(u: &mut Unifier) -> Scheme {
            let ($a,$b) = (u.fresh_tyvar(), u.fresh_tyvar());
            mk_scheme(&[$a,$b], &[], &[], thunk(curry!($($p),* => $ret)))
        }
    };
    // Pipe modes, 0 type vars: scheme!(ask: pipe [Ty::String] -> Ty::String);
    ($name:ident: pipe [$($p:expr),*] -> $ret:expr) => {
        pub fn $name(u: &mut Unifier) -> Scheme {
            let (m0,m1,ct) = fm(u, $ret);
            mk_scheme(&[], &[m0,m1], &[], thunk(curry_pipe!($($p),* => ct)))
        }
    };
    // Pipe modes, 1 type var
    ($name:ident<$tv:ident>: pipe [$($p:expr),*] -> $ret:expr) => {
        pub fn $name(u: &mut Unifier) -> Scheme {
            let $tv = u.fresh_tyvar();
            let (m0,m1,ct) = fm(u, $ret);
            mk_scheme(&[$tv], &[m0,m1], &[], thunk(curry_pipe!($($p),* => ct)))
        }
    };
}

/// Right-fold parameters into `fun(p₁, fun(p₂, …, pure(ret)))`.
macro_rules! curry {
    ($p:expr => $ret:expr) => { fun($p, pure($ret)) };
    ($p:expr, $($rest:expr),+ => $ret:expr) => { fun($p, curry!($($rest),+ => $ret)) };
}

/// Like [`curry!`] but the final position already carries a [`CompTy`]
/// (from [`fm`]), so we omit the `pure` wrapper.
macro_rules! curry_pipe {
    ($p:expr => $ret:expr) => { fun($p, $ret) };
    ($p:expr, $($rest:expr),+ => $ret:expr) => { fun($p, curry_pipe!($($rest),+ => $ret)) };
}

/// Reusable command signatures for builtins whose surface is not a
/// first-class curried value.
pub mod sig {
    use super::{
        ArgSig, ArgTemplate, BuiltinDiagnostic, BuiltinSig, CompTemplate, ModeTemplate, TyTemplate,
        scheme,
    };

    const ANY: ArgTemplate = ArgTemplate::Any;
    const STR: ArgTemplate = ArgTemplate::Ty(TyTemplate::String);
    const INT: ArgTemplate = ArgTemplate::Ty(TyTemplate::Int);
    const FLOAT: ArgTemplate = ArgTemplate::Ty(TyTemplate::Float);
    const BLOCK: ArgTemplate = ArgTemplate::BlockOrLambda;
    const BYTES_OR_INT_LIST: &[ArgTemplate] = &[
        ArgTemplate::Ty(TyTemplate::Bytes),
        ArgTemplate::Ty(TyTemplate::ListInt),
    ];

    const TWO_INTS: &[ArgTemplate] = &[INT, INT];
    const ONE_ANY: &[ArgTemplate] = &[ANY];
    const ONE_STR: &[ArgTemplate] = &[STR];
    const ONE_INT: &[ArgTemplate] = &[INT];
    const ONE_FLOAT: &[ArgTemplate] = &[FLOAT];
    const FLOAT_INT: &[ArgTemplate] = &[FLOAT, INT];
    const TO_BYTES_ARGS: &[ArgTemplate] = &[ArgTemplate::OneOf(BYTES_OR_INT_LIST)];
    const TO_LINES_ARGS: &[ArgTemplate] = &[ArgTemplate::Ty(TyTemplate::ListAny)];
    const NO_ARGS: &[ArgTemplate] = &[];
    const ALIAS_ARGS: &[ArgTemplate] = &[STR, BLOCK];

    const fn pure(value: TyTemplate) -> CompTemplate {
        CompTemplate::Pure(value)
    }

    const fn ret(input: ModeTemplate, output: ModeTemplate, value: TyTemplate) -> CompTemplate {
        CompTemplate::Return {
            input,
            output,
            value,
        }
    }

    const fn command(
        args: ArgSig,
        result: CompTemplate,
        value: Option<fn(&mut super::Unifier) -> super::Scheme>,
    ) -> BuiltinSig {
        BuiltinSig {
            args,
            result,
            value,
            diagnostic: BuiltinDiagnostic::None,
        }
    }

    pub const TERMINAL_CONTROL: BuiltinSig = command(
        ArgSig::Exact(NO_ARGS),
        ret(ModeTemplate::Fresh, ModeTemplate::Bytes, TyTemplate::Unit),
        None,
    );

    pub const RANGE: BuiltinSig = command(
        ArgSig::Exact(TWO_INTS),
        pure(TyTemplate::ListInt),
        Some(scheme::range),
    );

    pub const FROM_BYTES: BuiltinSig = command(
        ArgSig::Optional(ArgTemplate::Ty(TyTemplate::Bytes)),
        ret(ModeTemplate::Bytes, ModeTemplate::None, TyTemplate::Bytes),
        None,
    );

    pub const FROM_STRING: BuiltinSig = command(
        ArgSig::Optional(ANY),
        ret(ModeTemplate::Bytes, ModeTemplate::None, TyTemplate::String),
        None,
    );

    pub const FROM_JSON: BuiltinSig = command(
        ArgSig::Optional(ANY),
        ret(ModeTemplate::Bytes, ModeTemplate::None, TyTemplate::Any),
        None,
    );

    pub const FROM_LINES: BuiltinSig =
        command(ArgSig::Optional(ANY), CompTemplate::LinesStep, None);

    pub const TO_BYTES: BuiltinSig = command(
        ArgSig::DataLast(TO_BYTES_ARGS),
        ret(ModeTemplate::None, ModeTemplate::Bytes, TyTemplate::Bytes),
        None,
    );

    pub const TO_ANY_BYTES: BuiltinSig = command(
        ArgSig::DataLast(ONE_ANY),
        ret(ModeTemplate::None, ModeTemplate::Bytes, TyTemplate::Bytes),
        None,
    );

    pub const TO_LINE: BuiltinSig = command(
        ArgSig::DataLast(ONE_ANY),
        ret(ModeTemplate::None, ModeTemplate::Bytes, TyTemplate::Unit),
        None,
    );

    pub const TO_LINES: BuiltinSig = command(
        ArgSig::DataLast(TO_LINES_ARGS),
        ret(ModeTemplate::None, ModeTemplate::Bytes, TyTemplate::Bytes),
        None,
    );

    pub const CHDIR: BuiltinSig = command(ArgSig::Optional(STR), pure(TyTemplate::Unit), None);
    pub const PATH_BOOL: BuiltinSig = command(
        ArgSig::Exact(ONE_STR),
        pure(TyTemplate::Bool),
        Some(scheme::path_bool),
    );

    pub const INT_PARSE: BuiltinSig = command(
        ArgSig::Exact(ONE_ANY),
        pure(TyTemplate::Int),
        Some(scheme::any_to_int),
    );
    pub const FLOAT_PARSE: BuiltinSig = command(
        ArgSig::Exact(ONE_ANY),
        pure(TyTemplate::Float),
        Some(scheme::any_to_float),
    );
    pub const STR_PARSE: BuiltinSig = command(
        ArgSig::Exact(ONE_ANY),
        pure(TyTemplate::String),
        Some(scheme::any_to_string),
    );
    /// `round <x> <places>` — a Float and an Int dial, yielding a Float.
    pub const ROUND: BuiltinSig = command(ArgSig::Exact(FLOAT_INT), pure(TyTemplate::Float), None);
    /// `floor` / `ceil` / `trunc` — one Float in, the Int in that direction.
    pub const FLOAT_TO_INT: BuiltinSig =
        command(ArgSig::Exact(ONE_FLOAT), pure(TyTemplate::Int), None);

    pub const ALIAS: BuiltinSig = command(ArgSig::Exact(ALIAS_ARGS), pure(TyTemplate::Unit), None);
    pub const UNALIAS: BuiltinSig = command(ArgSig::Exact(ONE_STR), pure(TyTemplate::Unit), None);

    pub const HELP: BuiltinSig = command(
        ArgSig::Exact(NO_ARGS),
        ret(ModeTemplate::Fresh, ModeTemplate::Bytes, TyTemplate::Unit),
        None,
    );

    pub const EXPLAIN: BuiltinSig = command(
        ArgSig::Exact(ONE_STR),
        ret(ModeTemplate::Fresh, ModeTemplate::Bytes, TyTemplate::Unit),
        Some(scheme::explain_op),
    );

    pub const INT_TO_UNIT: BuiltinSig =
        command(ArgSig::Exact(ONE_INT), pure(TyTemplate::Unit), None);

    /// `fg`/`bg`/`disown`: zero or one Int.  A bare invocation defaults the
    /// job id to the most recent job (SPEC §18).
    pub const OPTIONAL_INT_TO_UNIT: BuiltinSig = command(
        ArgSig::Optional(ArgTemplate::Ty(TyTemplate::Int)),
        pure(TyTemplate::Unit),
        None,
    );

    pub const STRING_TO_UNIT: BuiltinSig =
        command(ArgSig::Exact(ONE_STR), pure(TyTemplate::Unit), None);

    pub const FAIL: BuiltinSig = BuiltinSig {
        args: ArgSig::Exact(ONE_ANY),
        result: CompTemplate::Never,
        value: Some(scheme::fail_op),
        diagnostic: BuiltinDiagnostic::FailStatusNonzero,
    };

    pub const TYPE_PROBE: BuiltinSig = BuiltinSig {
        args: ArgSig::Exact(ONE_ANY),
        result: ret(ModeTemplate::Fresh, ModeTemplate::Fresh, TyTemplate::Any),
        value: Some(scheme::type_probe),
        diagnostic: BuiltinDiagnostic::TypeProbe,
    };
}

/// Build a closed record type from a list of (label, type) pairs.
pub fn closed_record(fields: &[(&str, Ty)]) -> Ty {
    let mut row = Row::Empty;
    for (l, t) in fields.iter().rev() {
        row = Row::Extend(l.to_string(), Box::new(t.clone()), Box::new(row));
    }
    Ty::Record(row)
}

/// The error record `try` hands to its on-failure handler.  `message`
/// carries the failure text — the runtime error's message or the failing
/// external command's stderr decoded as UTF-8.  Per-command bytes are
/// not attached here (§10.1); use `audit` if forensic capture is wanted,
/// or `await`'s `stderr: Bytes` field for a captured concurrent block.
pub(super) fn try_error_record() -> Ty {
    closed_record(&[
        ("status", Ty::Int),
        ("cmd", Ty::String),
        ("message", Ty::String),
        ("line", Ty::Int),
        ("col", Ty::Int),
    ])
}

/// The audit-node record produced by `audit { … }`.  Mirrors the value
/// shape `ExecNode::to_value` materialises at runtime: every audit
/// frame's metadata plus the body's return value (typed `α`) and a list
/// of child nodes (each typed as a fresh map for forward compatibility).
pub(super) fn audit_record(value_ty: Ty, child_ty: Ty) -> Ty {
    closed_record(&[
        ("kind", Ty::String),
        ("cmd", Ty::String),
        ("args", Ty::List(Box::new(Ty::String))),
        ("status", Ty::Int),
        ("script", Ty::String),
        ("line", Ty::Int),
        ("col", Ty::Int),
        ("stdout", Ty::Bytes),
        ("stderr", Ty::Bytes),
        ("value", value_ty),
        ("children", Ty::List(Box::new(Ty::Map(Box::new(child_ty))))),
        ("start", Ty::Int),
        ("end", Ty::Int),
        ("principal", Ty::String),
    ])
}

/// The `{ value, stdout, stderr }` record returned by `await`/`race`.  The
/// block's return type α flows into `value`; stdout and stderr are the raw
/// byte buffers.  Failure is signalled by raising, not by a flag — wrap in
/// `try` to recover.  The block's exit status is not a field here; it lives
/// inside `poll`'s `` `err `` outcome when the block fails.
fn await_record(value_ty: Ty) -> Ty {
    closed_record(&[
        ("value", value_ty),
        ("stdout", Ty::Bytes),
        ("stderr", Ty::Bytes),
    ])
}

/// The `Settle α` record `poll` carries in its `` `settled `` arm:
/// `{stdout, stderr, outcome}`.  `stdout`/`stderr` are the bytes the block
/// wrote; `outcome` is the closed variant `` <ok: α | err: ErrRecord> ``,
/// where the `` `ok `` payload is the block's return value and the
/// `` `err `` payload is the same error record `try` hands its handler
/// ([`try_error_record`]) — the block's status lives inside it.
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

/// The `{stdout, stderr}` record `poll` carries in its `` `pending `` arm:
/// the bytes a still-running block has written *so far*, sampled
/// non-destructively (a cumulative snapshot).  Distinct from
/// [`settle_record`] — a pending poll has no outcome to report yet, only the
/// partial output accumulated to this point.
fn pending_record() -> Ty {
    closed_record(&[("stdout", Ty::Bytes), ("stderr", Ty::Bytes)])
}

/// The variant returned by `poll`, total over a finished block:
/// `` `settled `` carries the `Settle α` record ([`settle_record`]) for a
/// block that finished — returning, raising, or panicking — and
/// `` `pending `` carries the partial `{stdout, stderr}` record
/// ([`pending_record`]) — the bytes written so far — while it runs.  `poll`
/// is the non-blocking dual of `await`: rather than re-raising a failed
/// block, it reports it inside the `` `settled `` outcome's `` `err `` arm.
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

/// The record type returned by `file-info`.
///
/// Superset of
/// `fs_list_entry_ty` — same `name/type/size/mtime` plus access /
/// birth times, the readonly bit, and the symlink `target` (empty
/// string for non-symlinks).
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

/// Per-builtin scheme factories, one function per registered shape.
///
/// Each function allocates fresh unifier vars from `u` and returns a
/// fully-realised [`Scheme`] suitable for direct storage in the type
/// env or use at a call site.  Multiple registry entries that share a
/// shape (e.g. `upper` / `lower` / `dedent` / `shell-quote`) reuse the
/// same function from this module rather than duplicating the body.
///
/// Referenced from `builtin_registry!` entries via the `ty:` facet —
/// see [`BuiltinTypeRule::Scheme`].
pub mod scheme {
    use super::{
        CompTy, PipeMode, PipeSpec, Row, Scheme, Ty, Unifier, await_record, fs_file_info_ty,
        fs_list_entry_ty, fun, mk_scheme, poll_variant, pure, reducer_spec, thunk,
    };

    /// F[μ₀,μ₁] τ — for first-class builtins whose pipeline modes flow
    /// from the caller (e.g. `$ask`, `$source`).  The modes are fresh
    /// per-call vars so they can be pinned at each use site.
    fn fm(u: &mut Unifier, ty: Ty) -> (super::ModeVar, super::ModeVar, CompTy) {
        let (m0, m1) = (u.fresh_modevar(), u.fresh_modevar());
        let cty = CompTy::Return(
            PipeSpec {
                input: PipeMode::Var(m0),
                output: PipeMode::Var(m1),
            },
            Box::new(ty),
        );
        (m0, m1, cty)
    }

    // ── List operations ──────────────────────────────────────────────────

    scheme!(length<av>: [Ty::Var(av)] -> Ty::Int);

    /// `range :: Int → Int → F [Int]`
    pub fn range(_u: &mut Unifier) -> Scheme {
        mk_scheme(
            &[],
            &[],
            &[],
            thunk(fun(
                Ty::Int,
                fun(Ty::Int, pure(Ty::List(Box::new(Ty::Int)))),
            )),
        )
    }

    /// `surface :: ∀ρ. Variant ρ → F ()` — forward a tagged event to the
    /// host's structured-event sink.
    ///
    /// The row is open and otherwise
    /// unconstrained: any variant is accepted, and the host decides which
    /// tags it understands.
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

    /// Result computation type of a higher-order callback:
    /// `F[μ₀,μ₁] τ` with fresh, per-scheme-quantified pipeline modes.
    ///
    /// A callback body may itself read or write bytes — `map { echo $x }`
    /// is ordinary ral, where `echo` flushes to the visible stream while
    /// the list operation still returns a value.  The modes are therefore
    /// universally quantified, not pinned to `none`; mode unification is
    /// equality-strict (`docs/SPEC.md` §4.2.1), so a callback fixed to
    /// `F[none,none] τ` would reject any byte-output body.
    fn callback_result(u: &mut Unifier, ty: Ty) -> ([super::ModeVar; 2], CompTy) {
        let (m0, m1, cty) = fm(u, ty);
        ([m0, m1], cty)
    }

    /// `map :: ∀α β μ₀ μ₁. U(α → F[μ₀,μ₁] β) → [α] → F [β]`
    pub fn map_op(u: &mut Unifier) -> Scheme {
        let (av, bv) = (u.fresh_tyvar(), u.fresh_tyvar());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        let (cb_modes, cb_result) = callback_result(u, b.clone());
        mk_scheme(
            &[av, bv],
            &cb_modes,
            &[],
            thunk(fun(
                thunk(fun(a.clone(), cb_result)),
                fun(Ty::List(Box::new(a)), pure(Ty::List(Box::new(b)))),
            )),
        )
    }

    /// `filter :: ∀α μ₀ μ₁. U(α → F[μ₀,μ₁] Bool) → [α] → F [α]`
    pub fn filter_op(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        let (cb_modes, cb_result) = callback_result(u, Ty::Bool);
        mk_scheme(
            &[av],
            &cb_modes,
            &[],
            thunk(fun(
                thunk(fun(a.clone(), cb_result)),
                fun(Ty::List(Box::new(a.clone())), pure(Ty::List(Box::new(a)))),
            )),
        )
    }

    /// `each :: ∀α β μ₀ μ₁. U(α → F[μ₀,μ₁] β) → [α] → F Unit`
    pub fn each_op(u: &mut Unifier) -> Scheme {
        let (av, bv) = (u.fresh_tyvar(), u.fresh_tyvar());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        let (cb_modes, cb_result) = callback_result(u, b);
        mk_scheme(
            &[av, bv],
            &cb_modes,
            &[],
            thunk(fun(
                thunk(fun(a.clone(), cb_result)),
                fun(Ty::List(Box::new(a)), pure(Ty::Unit)),
            )),
        )
    }

    /// `fold :: ∀α β μ₀ μ₁. U(β → α → F[μ₀,μ₁] β) → β → [α] → F β`
    pub fn fold_op(u: &mut Unifier) -> Scheme {
        let (av, bv) = (u.fresh_tyvar(), u.fresh_tyvar());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        let (cb_modes, cb_result) = callback_result(u, b.clone());
        mk_scheme(
            &[av, bv],
            &cb_modes,
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

    /// `sort-list-by :: ∀α β μ₀ μ₁. U(α → F[μ₀,μ₁] β) → [α] → F [α]`
    pub fn sort_list_by(u: &mut Unifier) -> Scheme {
        let (av, bv) = (u.fresh_tyvar(), u.fresh_tyvar());
        let (a, b) = (Ty::Var(av), Ty::Var(bv));
        let (cb_modes, cb_result) = callback_result(u, b);
        mk_scheme(
            &[av, bv],
            &cb_modes,
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

    scheme!(path_bool: [Ty::String] -> Ty::Bool);

    // ── Streaming reducers ───────────────────────────────────────────────

    /// `fold-lines :: ∀α μ. U(α → Str → F[∅,μ] α) → α → F[Bytes,μ] α`
    ///
    /// The reducer reads its byte input line-by-line and threads the
    /// accumulator through the callback; whatever bytes the callback
    /// writes to stdout become the reducer's own byte output.  So the
    /// callback's output mode `μ` is the reducer's output mode: a pure
    /// callback (`return $acc`) keeps `μ = ∅`, leaving a value-producing
    /// decode `F[Bytes,∅] α`, while a callback that emits bytes (the
    /// `map-lines`/`filter-lines`/`each-line` wrappers `echo` per line)
    /// lifts the whole stage to `F[Bytes,Bytes] α`.
    pub fn fold_lines(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        let mu = u.fresh_modevar();
        let callback_result = CompTy::Return(
            PipeSpec {
                input: PipeMode::None,
                output: PipeMode::Var(mu),
            },
            Box::new(a.clone()),
        );
        mk_scheme(
            &[av],
            &[mu],
            &[],
            thunk(fun(
                thunk(fun(a.clone(), fun(Ty::String, callback_result))),
                fun(
                    a.clone(),
                    CompTy::Return(reducer_spec(PipeMode::Var(mu)), Box::new(a)),
                ),
            )),
        )
    }

    // ── Concurrency ──────────────────────────────────────────────────────

    /// `spawn :: ∀α μ₀ μ₁. U(F[μ₀,μ₁] α) → F (Handle α)`
    pub fn spawn(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        let (m0, m1, body) = fm(u, a.clone());
        mk_scheme(
            &[av],
            &[m0, m1],
            &[],
            thunk(fun(thunk(body), pure(Ty::Handle(Box::new(a))))),
        )
    }

    /// `watch :: ∀α μ₀ μ₁. String → U(F[μ₀,μ₁] α) → F (Handle α)`
    pub fn watch(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        let (m0, m1, body) = fm(u, a.clone());
        mk_scheme(
            &[av],
            &[m0, m1],
            &[],
            thunk(fun(
                Ty::String,
                fun(thunk(body), pure(Ty::Handle(Box::new(a)))),
            )),
        )
    }

    /// `service :: ∀α μ₀ μ₁. String → U(F[μ₀,μ₁] α) → F (Handle α)` —
    /// `watch`'s scheme, with the leading `String` now the mandatory birth
    /// description.
    ///
    /// The durable lease class is a runtime fact, invisible
    /// to the types.
    pub fn service(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let a = Ty::Var(av);
        let (m0, m1, body) = fm(u, a.clone());
        mk_scheme(
            &[av],
            &[m0, m1],
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

    /// `cancel` :: `∀α. Handle α → F Unit`
    pub fn cancel_op(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        mk_scheme(
            &[av],
            &[],
            &[],
            thunk(fun(Ty::Handle(Box::new(Ty::Var(av))), pure(Ty::Unit))),
        )
    }

    // ── Exit / fail ──────────────────────────────────────────────────────

    scheme!(exit_op: [Ty::Int] -> Ty::Unit);

    /// `fail :: ∀α ρ. [status: Int | ρ] → F α`.
    ///
    /// Always diverges; the
    /// result type is unconstrained.  Argument is an open record
    /// requiring at least `status: Int`; the row tail accepts arbitrary
    /// further fields (`message`, …) so a caught error record can be
    /// re-raised verbatim.  This record shape is enforced only by the
    /// value form `$fail` (this scheme); in command position `fail` takes
    /// a single `Any` argument (`sig::FAIL`), so only the literal
    /// `fail [status: 0]` is caught — by [`super::fail_status_is_zero_literal`]
    /// — and any other single argument (e.g. `fail 0`) is deferred to the
    /// runtime.
    pub fn fail_op(u: &mut Unifier) -> Scheme {
        let av = u.fresh_tyvar();
        let rho = u.fresh_row_var();
        let arg = Ty::Record(Row::Extend(
            "status".into(),
            Box::new(Ty::Int),
            Box::new(Row::Var(rho)),
        ));
        // `fail` diverges, so it constrains no pipeline channels: fresh,
        // quantified modes, matching the `CompTemplate::Never` projection in
        // `sig_pipe_spec`. `pure` would pin *no-channel* modes, which clash in a
        // branch-mode union — `if … { fail } else { <byte pipeline> }` would then
        // discard the live branch's byte mode (this is what broke `view`).
        let in_mode = u.fresh_modevar();
        let out_mode = u.fresh_modevar();
        let result = CompTy::Return(
            PipeSpec {
                input: PipeMode::Var(in_mode),
                output: PipeMode::Var(out_mode),
            },
            Box::new(Ty::Var(av)),
        );
        mk_scheme(&[av], &[in_mode, out_mode], &[rho], thunk(fun(arg, result)))
    }

    // ── First-class constants / queries ──────────────────────────────────

    scheme!(pure_string: pure Ty::String);

    scheme!(pure_bool: pure Ty::Bool);
    scheme!(explain_op: pipe [Ty::String] -> Ty::Unit);

    // ── First-class functions with pipeline modes ───────────────────────

    scheme!(ask: pipe [Ty::String] -> Ty::String);

    scheme!(type_probe<av>: pipe [Ty::Var(av)] -> Ty::Var(av));

    scheme!(any_to_int<av>: [Ty::Var(av)] -> Ty::Int);

    scheme!(any_to_float<av>: [Ty::Var(av)] -> Ty::Float);

    scheme!(any_to_string<av>: [Ty::Var(av)] -> Ty::String);

    scheme!(source_op<av>: pipe [Ty::String] -> Ty::Var(av));

    scheme!(use_op<av>: pipe [Ty::String] -> Ty::Map(Box::new(Ty::Var(av))));
}

/// Return a polymorphic type scheme for a builtin executable by name.
///
/// Consults `table`, the checked session's own builtin surface, so a name
/// resolves exactly against what that session can evaluate.  Returns `None`
/// when the name is unknown to `table` or its signature has no first-class
/// value form.
///
/// Fresh type/mode/row variables are allocated directly from `u`, so the
/// returned scheme can be stored in the environment or used at a call site
/// without any post-processing renaming step.
pub fn builtin_scheme(table: &BuiltinTable, name: &str, u: &mut Unifier) -> Option<Scheme> {
    match table.get(name)?.type_rule {
        BuiltinTypeRule::Scheme(_, factory) => Some(factory(u)),
        BuiltinTypeRule::Sig(sig) => sig.value.map(|f| f(u)),
    }
}

/// Return the formatted type string for a builtin, or `None` if unknown.
pub fn builtin_type_hint(table: &BuiltinTable, name: &str) -> Option<String> {
    let mut u = Unifier::new();
    let scheme = builtin_scheme(table, name, &mut u)?;
    Some(fmt_scheme(&scheme))
}

/// Number of value arguments the builtin's scheme declares (count of nested
/// `Fun` layers under the outer `Thunk`).
///
/// Used to η-expand first-class
/// builtin references (`$upper`) into curried lambda thunks.  `None` for
/// builtins without an arity — typically variadic ones like `echo` or
/// command-only dispatchers.
///
/// Delegates to the entry's own `fixed_arity`, off `table`.  In debug
/// builds, asserts that this matches the arity derived from the entry's
/// first-class scheme, when present — the arity and the scheme factory are
/// co-defined in the same `builtin_registry!` entry, so the assert catches
/// a typo where the two drift apart.
pub fn builtin_arity(table: &BuiltinTable, name: &str) -> Option<usize> {
    let arity = table.get(name).and_then(|e| e.fixed_arity());
    debug_assert!(
        check_arity_consistency(table, name, arity),
        "builtin '{name}': table arity ({arity:?}) ≠ scheme-derived arity",
    );
    arity
}

/// Cross-check `table`'s declared `arity:` against the `Fun`-nesting depth
/// derived from the entry's scheme factory (when there is one).  Returns
/// `true` when consistent — including command-only signatures where there
/// is no first-class scheme to check.
#[doc(hidden)]
pub fn check_arity_consistency(
    table: &BuiltinTable,
    name: &str,
    table_arity: Option<usize>,
) -> bool {
    let mut u = Unifier::new();
    let Some(scheme) = builtin_scheme(table, name, &mut u) else {
        return true;
    };
    let Some(_) = table_arity else {
        return true;
    };
    fn count(ct: &CompTy) -> usize {
        match ct {
            CompTy::Fun(_, body) => 1 + count(body),
            _ => 0,
        }
    }
    let scheme_arity = match &scheme.ty {
        Ty::Thunk(inner) => Some(count(inner)),
        _ => Some(0),
    };
    scheme_arity == table_arity
}

/// A per-key type schema — `fn(key, unifier) -> Option<Ty>`.
///
/// Drives [`super::infer::Inferencer::check_map_entry_fields`].  Returning
/// `None` for a key leaves that entry runtime-dispatched (still inferred
/// for side-effects, but not unified against anything).
pub type FieldSchema = fn(&str, &mut Unifier) -> Option<Ty>;

/// Schema for rc plugin entries `[plugin: Str, options: Map]`.
pub fn plugin_entry_field_ty(key: &str, u: &mut Unifier) -> Option<Ty> {
    match key {
        "plugin" => Some(Ty::String),
        "options" => Some(Ty::Map(Box::new(u.fresh_ty()))),
        _ => None,
    }
}

/// Detect the literal `fail [status: 0, ...]` shape.
///
/// `fail` requires a nonzero exit status (the runtime rejects status 0
/// at the builtin entry, see `builtins::misc::builtin_fail`).  When the
/// argument is a literal map whose `status` is the literal `0`, the
/// caller can produce a typecheck-time diagnostic without waiting for
/// the runtime check.  Dynamic shapes (computed status, spread args)
/// still defer to the runtime.
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
                crate::ir::Val::Int(0),
            ) if k == "status"
        ))
    )
}

/// The builtin-signature interpreter: turns a data-only [`BuiltinSig`] into
/// an inferred [`CompTy`], colocated with the templates it consumes.
impl Inferencer<'_> {
    fn ty_from_template(&mut self, template: TyTemplate) -> Ty {
        match template {
            TyTemplate::String => Ty::String,
            TyTemplate::Int => Ty::Int,
            TyTemplate::Float => Ty::Float,
            TyTemplate::Bool => Ty::Bool,
            TyTemplate::Bytes => Ty::Bytes,
            TyTemplate::Unit => Ty::Unit,
            TyTemplate::Any => self.ctx.unifier.fresh_ty(),
            TyTemplate::ListAny => {
                let elem = self.ctx.unifier.fresh_ty();
                Ty::List(Box::new(elem))
            }
            TyTemplate::ListInt => Ty::List(Box::new(Ty::Int)),
        }
    }

    fn builtin_sig_result(&mut self, sig: BuiltinSig) -> CompTy {
        let pipe = sig_pipe_spec(&sig.result, &mut self.ctx.unifier);
        let value = match sig.result {
            CompTemplate::Pure(ty) | CompTemplate::Return { value: ty, .. } => {
                self.ty_from_template(ty)
            }
            CompTemplate::Never => self.ctx.unifier.fresh_ty(),
            CompTemplate::LinesStep => self.lines_step_ty(),
        };
        CompTy::Return(pipe, Box::new(value))
    }

    fn unify_arg_template(&mut self, actual: &Ty, template: ArgTemplate) {
        match template {
            ArgTemplate::Any => {}
            ArgTemplate::Ty(ty) => {
                let expected = self.ty_from_template(ty);
                self.ctx
                    .unify_ty(actual, &expected, Reason::BuiltinTypedArg);
            }
            ArgTemplate::BlockOrLambda => {
                let result = self.ctx.unifier.fresh_comp_ty();
                let expected = Ty::Thunk(Box::new(result));
                self.ctx
                    .unify_ty(actual, &expected, Reason::BuiltinBlockArg);
            }
            ArgTemplate::OneOf(options) => {
                let resolved = self.ctx.unifier.apply_ty(actual);
                match resolved {
                    Ty::Bytes
                        if options
                            .iter()
                            .any(|o| matches!(o, ArgTemplate::Ty(TyTemplate::Bytes))) => {}
                    Ty::List(_)
                        if options
                            .iter()
                            .any(|o| matches!(o, ArgTemplate::Ty(TyTemplate::ListInt))) =>
                    {
                        self.ctx.unify_ty(
                            actual,
                            &Ty::List(Box::new(Ty::Int)),
                            Reason::BuiltinTypedArg,
                        );
                    }
                    Ty::Var(_)
                        if options
                            .iter()
                            .any(|o| matches!(o, ArgTemplate::Ty(TyTemplate::Bytes))) =>
                    {
                        self.ctx
                            .unify_ty(actual, &Ty::Bytes, Reason::BuiltinTypedArg);
                    }
                    _ => {
                        if let Some(ArgTemplate::Ty(ty)) = options.first() {
                            let expected = self.ty_from_template(*ty);
                            self.ctx
                                .unify_ty(actual, &expected, Reason::BuiltinTypedArg);
                        }
                    }
                }
            }
        }
    }

    pub(super) fn apply_builtin_sig(&mut self, sig: BuiltinSig, args: &crate::ir::Args) -> CompTy {
        if sig.diagnostic == BuiltinDiagnostic::FailStatusNonzero
            && fail_status_is_zero_literal(args)
        {
            self.ctx.diagnose(TypeErrorKind::FailStatusZero);
        }

        let mut type_probe_arg = None;
        match crate::ir::args::positional(args) {
            Some(positional) => match sig.args {
                ArgSig::Exact(expected) | ArgSig::DataLast(expected) => {
                    let missing_data_last = matches!(sig.args, ArgSig::DataLast(_))
                        && positional.len() + 1 == expected.len();
                    if positional.len() != expected.len() && !missing_data_last {
                        self.ctx.diagnose(TypeErrorKind::BuiltinArity {
                            expected: expected.len(),
                            got: positional.len(),
                            at_most: false,
                        });
                    }
                    for (arg, template) in positional.iter().zip(expected.iter()) {
                        let actual = self.infer_val(arg);
                        if sig.diagnostic == BuiltinDiagnostic::TypeProbe {
                            type_probe_arg = Some(actual.clone());
                        }
                        self.unify_arg_template(&actual, *template);
                    }
                    for arg in positional.iter().skip(expected.len()) {
                        let _ = self.infer_val(arg);
                    }
                }
                ArgSig::Optional(template) => {
                    if positional.len() > 1 {
                        self.ctx.diagnose(TypeErrorKind::BuiltinArity {
                            expected: 1,
                            got: positional.len(),
                            at_most: true,
                        });
                    }
                    for arg in &positional {
                        let actual = self.infer_val(arg);
                        self.unify_arg_template(&actual, template);
                    }
                }
                ArgSig::Any => self.infer_args(args),
            },
            None => self.infer_args(args),
        }

        let result = self.builtin_sig_result(sig);
        if sig.diagnostic == BuiltinDiagnostic::TypeProbe
            && let Some(arg_ty) = type_probe_arg
        {
            // `_type` is `α → F α`: thread the argument's type through to
            // the result so the probe is transparent to downstream
            // inference, then print the resolved α.
            if let CompTy::Return(_, value_ty) = &result {
                self.ctx.unify_ty(value_ty, &arg_ty, Reason::TypeProbe);
            }
            let resolved = self.ctx.unifier.apply_ty(&arg_ty);
            let pos = self
                .ctx
                .pos
                .map(|sp| format!("@{}..{}: ", sp.start, sp.end))
                .unwrap_or_default();
            eprintln!("_type: {}{}", pos, fmt_ty(&resolved));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn registry_and_scheme_arity_agree() {
        let table = crate::builtins::core_builtin_table();
        for name in table.names() {
            let _ = super::builtin_arity(&table, name);
        }
        assert_eq!(super::builtin_arity(&table, "values"), None);
    }
}
