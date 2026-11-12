//! Type-checker side of the builtin registry.
//!
//! Value rows select a typing rule through `builtin_registry!`'s `ty:` field;
//! argv rows share the named-handler type by construction. Here live those
//! rules, the handler types and the record shapes the builtins share.
//!
//! Scheme factories draw their variables from a local [`Binders`] supply,
//! independent of any inference session.

use super::error::Standing;
use crate::fact::{DirEntry, FileInfo};
use crate::fact::{ErrorRecord, Io, Observation, Receipt};
use crate::protocol::probe::WorkerRow;
use crate::ty::{
    Binders, Closed, CompTy, CompTyNode, Grade, Kind, Row, Ty, TyNode, Typed, closed_record,
    closed_variant,
};
use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

/// A value builtin's type, under binders drawn from the supplied scope.
pub(crate) type BuiltinTypeRule = fn(&mut Binders) -> Ty;

/// Extra non-typing behaviour a builtin's [`Decl`] carries: which diagnostic
/// an over-application or a literal misuse earns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinDiagnostic {
    None,
    /// A `from-*` decoder: an argument is not an arity slip but a misreading
    /// of where the bytes come from.
    Decoder,
}

/// Which of ral's two argument conventions a manifest row uses.  The manifest
/// is authored as two, and what a name can do follows from which half it is in
/// rather than from its arity.
#[derive(Debug, Clone, Copy)]
pub enum Convention {
    /// Curried application at the arity the row's type declares, and
    /// first-class as `$name`: the row seeds the base env scope as a
    /// `Value::Native(entry, [])`; partial applications hold their arguments.
    Value(BuiltinTypeRule),
    /// An argv base frame: `[String] → Command`, intercepted, stacked, and
    /// never a value. `^name` bypasses it. Its argv words are gated, not
    /// unified with `String`; the body receives the values themselves.
    Argv,
}

#[derive(Clone, Copy)]
pub(crate) enum Arm<'a> {
    Named(&'a str),
    CatchAll,
}

impl Arm<'_> {
    pub(crate) fn shape(self, body: CompTy) -> CompTy {
        match self {
            Self::Named(_) => CompTy::arrows([Ty::argv()], body),
            Self::CatchAll => CompTy::arrows([Ty::string(), Ty::argv()], body),
        }
    }

    pub(crate) fn ty(self) -> CompTy {
        match self {
            Self::Named(_) => handler(),
            Self::CatchAll => self.shape(CompTy::command()),
        }
    }

    pub(crate) fn standing(self) -> Standing {
        match self {
            Self::Named(name) => Standing::Command(name.into()),
            Self::CatchAll => Standing::EveryCommand,
        }
    }
}

fn handler() -> CompTy {
    CompTy::arrows([Ty::argv()], CompTy::command())
}

pub(super) fn handlers_ty() -> Ty {
    Ty::map(Ty::thunk(handler()))
}

/// `true` and `false` — language-given names in every base scope, live and
/// hydrated alike, though they are not manifest entries.  The checker types
/// them `Bool`.
pub(crate) const LANGUAGE_CONSTANTS: [(&str, bool); 2] = [("true", true), ("false", false)];

/// What a row's scheme fixes once: its curry depth and whether it settles
/// at `Unit`.
#[derive(Debug, Clone, Copy)]
struct Spine {
    arity: usize,
    settles_at_unit: bool,
}

impl Spine {
    fn of(scheme: &Closed) -> Self {
        fn settled(ct: &CompTy) -> (usize, Option<&Ty>) {
            match ct.node() {
                CompTyNode::Fun(_, body) => {
                    let (n, ty) = settled(body);
                    (n + 1, ty)
                }
                CompTyNode::Return(_, ty) => (0, Some(ty)),
                CompTyNode::Var(_) => (0, None),
            }
        }
        match scheme.ty.node() {
            TyNode::Thunk(inner) => {
                let (arity, ty) = settled(inner);
                Self {
                    arity,
                    settles_at_unit: matches!(ty.map(Ty::node), Some(TyNode::Unit)),
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
    /// Which diagnostic an over-application or a literal misuse earns; `None`
    /// for the overwhelming majority of rows.
    pub(crate) diagnostic: BuiltinDiagnostic,
    /// Set only by the boundary constructor: a call of the row carries a
    /// [`crate::ty::Site`].
    pub(crate) boundary: bool,
    scheme: OnceLock<Closed>,
    spine: OnceLock<Spine>,
}

impl Decl {
    pub(crate) const fn new(
        name: Cow<'static, str>,
        convention: Convention,
        doc: &'static str,
        boundary: bool,
    ) -> Self {
        Self {
            name,
            convention,
            doc,
            diagnostic: BuiltinDiagnostic::None,
            boundary,
            scheme: OnceLock::new(),
            spine: OnceLock::new(),
        }
    }

    pub fn is_boundary(&self) -> bool {
        self.boundary
    }

    /// The row's closed type; value rows quantify ordinary result grades.
    pub fn scheme(&self) -> &Closed {
        self.scheme.get_or_init(|| {
            /// `body` with a `Value` grade past its arrows made a fresh grade
            /// variable.
            fn quantified(body: &CompTy, binders: &mut Binders) -> CompTy {
                match body.node() {
                    CompTyNode::Fun(param, rest) => {
                        CompTy::fun(param.clone(), quantified(rest, binders))
                    }
                    CompTyNode::Return(Grade::Value, ty) => {
                        CompTy::ret(Grade::Var(binders.grade()), ty.clone())
                    }
                    CompTyNode::Return(..) | CompTyNode::Var(_) => body.clone(),
                }
            }
            Binders::close(|binders| match self.convention {
                Convention::Argv => Ty::thunk(Arm::Named(&self.name).ty()),
                Convention::Value(rule) => {
                    let ty = rule(binders);
                    match ty.node() {
                        TyNode::Thunk(body) if !self.is_boundary() => {
                            Ty::thunk(quantified(body, binders))
                        }
                        _ => ty,
                    }
                }
            })
        })
    }

    fn spine(&self) -> Spine {
        *self.spine.get_or_init(|| Spine::of(self.scheme()))
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
    /// reference reach. `None` for a base frame, reached only in bare command
    /// position through the handler stack; `^name` bypasses it.
    pub fn value(&self, name: &str) -> Option<&Decl> {
        self.rows()
            .find(|decl| decl.name == name && matches!(decl.convention, Convention::Value(_)))
    }

    /// The base-frame row for `name`, independent of installed handlers.
    pub(crate) fn base_frame(&self, name: &str) -> Option<&Decl> {
        self.base_frames().find(|decl| decl.name == name)
    }

    /// Every base-frame row in Σ.
    pub(crate) fn base_frames(&self) -> impl Iterator<Item = &Decl> {
        self.rows()
            .filter(|decl| matches!(decl.convention, Convention::Argv))
    }

    /// Names a `$name` reference reaches: the value rows.
    pub(crate) fn value_names(&self) -> impl Iterator<Item = &str> {
        self.rows()
            .filter(|decl| matches!(decl.convention, Convention::Value(_)))
            .map(|decl| decl.name.as_ref())
    }

    /// Names of installed builtins, newest installed set first.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.rows().map(|decl| decl.name.as_ref())
    }
}

pub fn thunk(cty: CompTy) -> Ty {
    Ty::thunk(cty)
}
pub fn fun(param: Ty, body: CompTy) -> CompTy {
    CompTy::fun(param, body)
}
pub fn pure(ty: Ty) -> CompTy {
    CompTy::pure(ty)
}
pub fn command() -> CompTy {
    CompTy::command()
}

// ── Scheme DSL ──────────────────────────────────────────────────────
//
// `scheme!` writes a builtin's polytype declaratively: `<tv>` draws fresh
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
// members that a value row already names — and a row is a caller, so no
// member of this family can be stranded silently the way a loose function can.

macro_rules! scheme {
    // scheme!(help: writes);
    ($name:ident: writes) => { scheme!(@ $name: [] => command()); };
    // scheme!(temp_path: pure Ty::string());
    ($name:ident: pure $ret:expr) => { scheme!(@ $name: [] => pure($ret)); };
    // scheme!(to_bytes: [Ty::bytes()] -> writes);
    ($name:ident: [$($p:expr),*] -> writes) => { scheme!(@ $name: [$($p),*] => command()); };
    // scheme!(str_to_str: [Ty::string()] -> Ty::string());
    ($name:ident: [$($p:expr),*] -> $ret:expr) => { scheme!(@ $name: [$($p),*] => pure($ret)); };
    // scheme!(to_line<av: Kind::DATA>: [Ty::var(av)] -> writes);
    ($name:ident<$($tv:ident: $kind:path),+>: [$($p:expr),*] -> writes) => {
        scheme!(@ $name<$($tv: $kind),+>: [$($p),*] => command());
    };
    // scheme!(length<av: Kind::SIZED>: [Ty::var(av)] -> Ty::int());
    // scheme!(has<av: Kind::ANY, bv: Kind::ANY>: [..] -> ..): any number.
    ($name:ident<$($tv:ident: $kind:path),+>: [$($p:expr),*] -> $ret:expr) => {
        scheme!(@ $name<$($tv: $kind),+>: [$($p),*] => pure($ret));
    };
    // The two expansions: `tail` past the curried parameters, under no type
    // variable, or one of the kind declared.
    (@ $name:ident: [$($p:expr),*] => $tail:expr) => {
        pub fn $name(_: &mut Binders) -> Ty {
            thunk(CompTy::arrows([$($p),*], $tail))
        }
    };
    (@ $name:ident<$($tv:ident: $kind:path),+>: [$($p:expr),*] => $tail:expr) => {
        pub fn $name(binders: &mut Binders) -> Ty {
            $(let $tv = binders.ty($kind);)+
            thunk(CompTy::arrows([$($p),*], $tail))
        }
    };
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
    closed_variant(&[
        (crate::fact::tag::OK, value_ty),
        (crate::fact::tag::ERR, ErrorRecord::ty()),
    ])
}

/// The `{stdout, stderr, elapsed, outcome}` record `poll` carries in its
/// `` `ready `` arm.  The `` `err `` payload is the very record `try` hands
/// its handler, so the block's status lives inside it.
fn settle_record(value_ty: Ty) -> Ty {
    Io::ty()
        .extend("elapsed", Ty::float())
        .extend("outcome", outcome_ty(value_ty))
}

/// The variant `poll` returns: [`settle_record`] in `` `ready `` once the block
/// has finished with an unclaimed result, what it has written so far while it
/// runs in `` `running `` (a cumulative, non-destructive snapshot), and
/// `[elapsed: Float]` in `` `stopping `` or `` `consumed `` once its result has
/// been claimed.  In `` `ready ``, an ordinary failure or scope stop settles
/// inside `` `err ``; a panic, an `exit` and a foreign stop are re-raised.
pub(crate) fn poll_variant(value_ty: Ty) -> Ty {
    closed_variant(&[
        ("running", Io::ty().extend("elapsed", Ty::float())),
        ("stopping", closed_record(&[("elapsed", Ty::float())])),
        ("ready", settle_record(value_ty)),
        ("consumed", closed_record(&[("elapsed", Ty::float())])),
    ])
}

/// Per-builtin scheme factories, one function per registered *shape*: entries
/// that share one (`upper`, `lower`, `dedent`, `ral-quote`) reuse a single
/// function here rather than duplicating the body.
pub mod scheme {
    use super::{
        Binders, CompTy, DirEntry, ErrorRecord, FileInfo, Grade, Kind, Receipt, Row, Ty, WorkerRow,
        command, fun, poll_variant, pure, thunk,
    };
    use crate::ty::Typed as _;

    // ── List operations ──────────────────────────────────────────────────

    scheme!(length<av: Kind::SIZED>: [Ty::var(av)] -> Ty::int());
    scheme!(nth<av: Kind::ANY>: [Ty::int(), Ty::list(Ty::var(av))] -> Ty::var(av));

    /// `surface :: ∀ρ. Variant ρ → F ()` — forward a tagged event to the host's
    /// event sink.  The row stays open: the host decides which tags it knows.
    ///
    /// `pub`: a host declares its own [`BuiltinEntry`](crate::shell::runtime::BuiltinEntry)
    /// over this scheme under its own name; see
    /// [`crate::shell::builtins::SURFACE_BUILTIN`].
    pub fn surface_op(binders: &mut Binders) -> Ty {
        let row = binders.row(false);
        thunk(fun(Ty::variant(Row::var(row)), pure(Ty::unit())))
    }

    // keys :: ∀α. Map<α> → F [Str]
    scheme!(keys<av: Kind::ANY>: [Ty::map(Ty::var(av))] -> Ty::list(Ty::string()));

    // has :: ∀α. Map<α> → Str → F Bool
    scheme!(has<av: Kind::ANY>: [Ty::map(Ty::var(av)), Ty::string()] -> Ty::bool());

    /// `map :: ∀ε ν α β. U(α → F^ε β) → [α] → F^ν [β]` — outer ν quantified by `Decl::scheme`.
    pub(crate) fn map_op(binders: &mut Binders) -> Ty {
        let (a, b) = (
            Ty::var(binders.ty(Kind::ANY)),
            Ty::var(binders.ty(Kind::ANY)),
        );
        let step = CompTy::ret(Grade::Var(binders.grade()), b.clone());
        thunk(fun(
            thunk(fun(a.clone(), step)),
            fun(Ty::list(a), pure(Ty::list(b))),
        ))
    }

    // filter :: ∀α. U(α → F Bool) → [α] → F [α]
    scheme!(filter_op<av: Kind::ANY>: [
        thunk(fun(Ty::var(av), pure(Ty::bool()))),
        Ty::list(Ty::var(av))
    ] -> Ty::list(Ty::var(av)));

    /// `each :: ∀ε α β. U(α → F^ε β) → [α] → F Unit` — the block's output
    /// streams, whatever it produces.
    pub(crate) fn each_op(binders: &mut Binders) -> Ty {
        let (a, b) = (
            Ty::var(binders.ty(Kind::ANY)),
            Ty::var(binders.ty(Kind::ANY)),
        );
        let step = CompTy::ret(Grade::Var(binders.grade()), b);
        thunk(fun(
            thunk(fun(a.clone(), step)),
            fun(Ty::list(a), pure(Ty::unit())),
        ))
    }

    /// `fold :: ∀ε α β. U(β → α → F^ε β) → β → [α] → F β`
    pub(crate) fn fold_op(binders: &mut Binders) -> Ty {
        let (a, b) = (
            Ty::var(binders.ty(Kind::ANY)),
            Ty::var(binders.ty(Kind::ANY)),
        );
        let step = CompTy::ret(Grade::Var(binders.grade()), b.clone());
        thunk(fun(
            thunk(fun(b.clone(), fun(a.clone(), step))),
            fun(b.clone(), fun(Ty::list(a), pure(b))),
        ))
    }

    // sort-list-by :: ∀α β:comparable. U(α → F β) → [α] → F [α]
    scheme!(sort_list_by<av: Kind::ANY, bv: Kind::COMPARABLE>: [
        thunk(fun(Ty::var(av), pure(Ty::var(bv)))),
        Ty::list(Ty::var(av))
    ] -> Ty::list(Ty::var(av)));

    // ── Strings & paths ──────────────────────────────────────────────────

    scheme!(str_to_str: [Ty::string()] -> Ty::string());

    scheme!(str_to_strs: [Ty::string()] -> Ty::list(Ty::string()));

    scheme!(re_match: [Ty::string(), Ty::string()] -> Ty::bool());

    scheme!(re_find_match: [Ty::string(), Ty::string()] -> Ty::string());

    scheme!(re_split: [Ty::string(), Ty::string()] -> Ty::list(Ty::string()));

    scheme!(replace_3: [Ty::string(), Ty::string(), Ty::string()] -> Ty::string());

    scheme!(slice: [Ty::string(), Ty::int(), Ty::int()] -> Ty::string());

    // intercalate :: ∀α. Str → [α] → F Str
    scheme!(intercalate<av: Kind::ANY>: [Ty::string(), Ty::list(Ty::var(av))] -> Ty::string());

    // ── File system & paths ──────────────────────────────────────────────

    // list-dir :: Str → F [{name, type, size, mtime}]
    scheme!(list_dir: [Ty::string()] -> Ty::list(DirEntry::ty()));

    // file-info :: Str → F {…full stat…}
    scheme!(file_info: [Ty::string()] -> FileInfo::ty());

    scheme!(temp_path: pure Ty::string());

    scheme!(glob: [Ty::string()] -> Ty::list(Ty::string()));

    // ── Streaming reducers ───────────────────────────────────────────────

    /// `fold-lines :: ∀ε α. U(α → Str → F^ε α) → α → F α`
    pub(crate) fn fold_lines(binders: &mut Binders) -> Ty {
        let a = Ty::var(binders.ty(Kind::ANY));
        let step = CompTy::ret(Grade::Var(binders.grade()), a.clone());
        thunk(fun(
            thunk(fun(a.clone(), fun(Ty::string(), step))),
            fun(a.clone(), pure(a)),
        ))
    }

    // ── Concurrency ──────────────────────────────────────────────────────

    /// `∀ε α. body(Handle^ε α, F^ε α, α)`: a scheme over one handle, its
    /// grade and value quantified.
    fn on_handle(binders: &mut Binders, body: impl FnOnce(Ty, CompTy, Ty) -> CompTy) -> Ty {
        let (a, e) = (Ty::var(binders.ty(Kind::ANY)), Grade::Var(binders.grade()));
        let handle = Ty::handle(e, a.clone());
        let produce = CompTy::ret(e, a.clone());
        thunk(body(handle, produce, a))
    }

    /// `watch :: ∀ε α. String → U(F^ε α) → F (Handle^ε α)`: the block runs
    /// in a worker, its lines streamed, whatever it produces.
    pub(crate) fn watch(binders: &mut Binders) -> Ty {
        on_handle(binders, |handle, body, _| {
            fun(Ty::string(), fun(thunk(body), pure(handle)))
        })
    }

    /// `is-done :: ∀ε α. Handle^ε α → F Bool`: whether execution has ended.
    pub(crate) fn is_done(binders: &mut Binders) -> Ty {
        on_handle(binders, |handle, _, _| fun(handle, pure(Ty::bool())))
    }

    /// `await :: ∀ε α. Handle^ε α → F^ε α`: the block, run here.
    pub(crate) fn await_op(binders: &mut Binders) -> Ty {
        on_handle(binders, |handle, body, _| fun(handle, body))
    }

    /// `poll :: ∀ε α. Handle^ε α → F <running: {stdout, stderr, elapsed} | stopping: {elapsed} | ready: {stdout, stderr, elapsed, outcome: <ok: α | err: ErrRecord>} | consumed: {elapsed}>`
    pub(crate) fn poll(binders: &mut Binders) -> Ty {
        on_handle(binders, |handle, _, a| fun(handle, pure(poll_variant(a))))
    }

    /// `race :: ∀ε α. [Handle^ε α] → F^ε α`: the winner, run here.
    pub(crate) fn race(binders: &mut Binders) -> Ty {
        on_handle(binders, |handle, body, _| fun(Ty::list(handle), body))
    }

    /// `first-settled :: ∀ε α. [Handle^ε α] → F Int`: which settled first.
    pub(crate) fn first_settled(binders: &mut Binders) -> Ty {
        on_handle(binders, |handle, _, _| {
            fun(Ty::list(handle), pure(Ty::int()))
        })
    }

    /// `cancel :: ∀ε α. Handle^ε α → F Unit`
    pub(crate) fn cancel_op(binders: &mut Binders) -> Ty {
        on_handle(binders, |handle, _, _| fun(handle, pure(Ty::unit())))
    }

    // workers :: F [WorkerRow]
    scheme!(workers_op: pure Ty::list(WorkerRow::ty()));

    scheme!(detach: [Ty::string(), Ty::string(), Ty::argv()] -> Receipt::ty());

    // ── First-class constants / queries ──────────────────────────────────

    scheme!(pure_string: pure Ty::string());

    scheme!(pure_int: pure Ty::int());

    scheme!(pure_strs: pure Ty::list(Ty::string()));

    scheme!(pure_bool: pure Ty::bool());

    // ── Host-backed queries ───────────────────────────────────────────────

    scheme!(ask: [Ty::string()] -> Ty::string());

    // ── Terminal, help & encoders ────────────────────────────────────────
    //
    // Each is a command: its value is what it writes.

    scheme!(terminal_control: writes);
    scheme!(help: writes);
    scheme!(explain: [Ty::string()] -> writes);
    scheme!(to_bytes: [Ty::bytes()] -> writes);
    scheme!(ints_to_bytes: [Ty::list(Ty::int())] -> writes);
    scheme!(to_any_bytes<av: Kind::DATA>: [Ty::var(av)] -> writes);
    scheme!(to_line<av: Kind::DATA>: [Ty::var(av)] -> writes);
    scheme!(to_lines<av: Kind::DATA>: [Ty::list(Ty::var(av))] -> writes);
    scheme!(to_csv<av: Kind::DATA>: [Ty::list(Ty::var(av))] -> writes);

    // ── Decoders ─────────────────────────────────────────────────────────
    //
    // Nullary: the bytes come from the channel, not an argument.

    scheme!(from_bytes: pure Ty::bytes());
    scheme!(from_string: pure Ty::string());
    scheme!(from_lines: pure Ty::list(Ty::string()));
    /// `from-json`, `env` :: ∀α:data. F α.
    pub fn from_json(binders: &mut Binders) -> Ty {
        thunk(pure(Ty::var(binders.ty(Kind::DATA))))
    }

    /// `from-json-at` :: ∀α:data. [String] → F α.
    pub fn from_json_at(binders: &mut Binders) -> Ty {
        let tokens = Ty::list(Ty::string());
        thunk(fun(tokens, pure(Ty::var(binders.ty(Kind::DATA)))))
    }

    /// `from-jsonl`, `from-csv` :: ∀α:data. F \[α\].
    pub fn from_jsonl(binders: &mut Binders) -> Ty {
        thunk(pure(Ty::list(Ty::var(binders.ty(Kind::DATA)))))
    }

    // ── Range, paths, parsing ────────────────────────────────────────────

    scheme!(range: [Ty::int(), Ty::int()] -> Ty::list(Ty::int()));
    scheme!(chdir: [Ty::string()] -> Ty::unit());
    scheme!(path_bool: [Ty::string()] -> Ty::bool());
    scheme!(int_parse<av: Kind::COMPARABLE>: [Ty::var(av)] -> Ty::int());
    scheme!(float_parse<av: Kind::COMPARABLE>: [Ty::var(av)] -> Ty::float());
    scheme!(str_parse<av: Kind::DATA>: [Ty::var(av)] -> Ty::string());
    scheme!(round: [Ty::float(), Ty::int()] -> Ty::float());
    // Shared by `floor`, `ceil`, and `trunc`.
    scheme!(float_to_int: [Ty::float()] -> Ty::int());

    scheme!(string_to_unit: [Ty::string()] -> Ty::unit());

    // ── Divergence ───────────────────────────────────────────────────────

    /// `∀ε α. F^ε α`: a divergent computation inhabits every producer type,
    /// so it joins whatever the other arm of an `if` is, a command included.
    fn divergent(binders: &mut Binders) -> CompTy {
        let a = Ty::var(binders.ty(Kind::ANY));
        CompTy::ret(Grade::Var(binders.grade()), a)
    }

    pub(crate) fn fail(binders: &mut Binders) -> Ty {
        thunk(fun(Ty::string(), divergent(binders)))
    }

    pub(crate) fn raise(binders: &mut Binders) -> Ty {
        thunk(fun(ErrorRecord::ty(), divergent(binders)))
    }

    /// `exit`/`quit` :: ∀ε α. Int → F^ε α — a status, and no return.
    /// The elaborator sugars bare `exit` to `exit 0`.
    pub(crate) fn exit(binders: &mut Binders) -> Ty {
        thunk(fun(Ty::int(), divergent(binders)))
    }

    /// `∀ε α. F^ε α` — nullary; for a host builtin that never returns (a
    /// test-only Rust panic trigger, say).
    pub fn diverges(binders: &mut Binders) -> Ty {
        thunk(divergent(binders))
    }
}

/// The formatted type of any manifest row, either half.
///
/// What `help` and `explain` print: a base frame's argv type is worth printing
/// even though no `$name` can hold it.  `None` only for a name the manifest
/// does not carry.
pub fn builtin_type_hint(manifest: &Manifest, name: &str) -> Option<String> {
    Some(manifest.get(name)?.scheme().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_argv_row_has_a_fixed_command_scheme() {
        let decl = Decl::new(Cow::Borrowed("host"), Convention::Argv, "", false);
        let expected = Ty::thunk(CompTy::arrows([Ty::argv()], CompTy::command()));
        assert_eq!(decl.scheme().ty, expected);
        assert_eq!(handlers_ty(), Ty::map(expected));
    }

    #[test]
    fn value_unit_results_instantiate_at_independent_grades() {
        let decl = Decl::new(
            Cow::Borrowed("host"),
            Convention::Value(scheme::string_to_unit),
            "",
            false,
        );
        let mut unifier = super::super::Unifier::new();
        for grade in [Grade::Value, Grade::Output] {
            let instance = super::super::instantiate(&mut unifier, decl.scheme());
            let wanted = thunk(CompTy::arrows(
                [Ty::string()],
                CompTy::ret(grade, Ty::unit()),
            ));
            unifier
                .unify_ty(&instance, &wanted)
                .expect("independent result grade");
        }
    }

    fn set(names: &[&'static str]) -> Arc<[Decl]> {
        names
            .iter()
            .map(|name| {
                Decl::new(
                    Cow::Borrowed(*name),
                    Convention::Value(scheme::str_to_str),
                    "",
                    false,
                )
            })
            .collect()
    }

    #[test]
    #[should_panic(expected = "installed twice")]
    fn a_set_repeating_a_name_is_refused() {
        Manifest::default().push(set(&["a", "a"]));
    }

    #[test]
    #[should_panic(expected = "conflicts with an installed builtin")]
    fn a_set_colliding_with_an_installed_name_is_refused() {
        let mut manifest = Manifest::default();
        manifest.push(set(&["a"]));
        manifest.push(set(&["b", "a"]));
    }

    #[test]
    fn a_manifest_handle_shares_its_rows_and_their_cached_schemes() {
        let mut manifest = Manifest::default();
        manifest.push(set(&["a"]));
        let handle = manifest.clone();
        let warmed = handle.value("a").expect("row").scheme();
        let cached = manifest.value("a").expect("row").scheme.get();
        assert!(cached.is_some_and(|cached| std::ptr::eq(cached, warmed)));
    }

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
