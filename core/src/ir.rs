//! Call-by-push-value intermediate representation: the target of
//! elaboration ([`crate::elaborator`]), the input to evaluation.
//!
//! [`Val`] is inert data and [`Comp`] is effectful, so a value can never
//! diverge or perform I/O.  The [`Spanned`] wrapper puts a source range on
//! every [`Comp`] and on each sub-`Val` position the typechecker narrows
//! onto, so a span rides with the value rather than the parent; `None`
//! means the node is synthetic — a builtin, the prelude, generated code.

use crate::path::tilde::TildePath;
use crate::source::Spanned;
use crate::syntax::ast::{BinaryOp, Pattern, RedirectMode};

/// A [`crate::syntax::ast::Pattern`] whose map-pattern defaults are already
/// elaborated to computations: no parser syntax survives elaboration.
pub type IrPattern = Pattern<Arc<Comp>>;
pub type Param = IrPattern;

// ── Values ──────────────────────────────────────────────────────────────
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The head word of a command, in the shape the source wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandName {
    Bare(String),
    /// Slash-bearing literal path: skips the lookup chain, exec'd as written.
    Path(String),
    /// Tilde-headed path, carried unexpanded until command resolution.
    TildePath(TildePath),
}

impl CommandName {
    pub fn bare(&self) -> Option<&str> {
        match self {
            Self::Bare(name) => Some(name),
            Self::Path(_) | Self::TildePath(_) => None,
        }
    }
}

/// The CBPV value category: inert data, requiring no evaluation.  The typed
/// literals exist so that `$[…]` lowers into plain `Bind` sequences rather
/// than round-tripping through string parsing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Val {
    Unit,
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Variable(String),
    /// A suspended computation, eliminated by [`CompKind::Force`].
    Thunk(Arc<Comp>),
    List(Vec<ValListElem>),
    Map(Vec<ValMapEntry>),
    /// `` `label `` or `` `label payload ``; the label is stored without
    /// its leading backtick.
    Variant {
        label: String,
        payload: Option<Box<Self>>,
    },
    TildePath(TildePath),
}

impl Val {
    /// Classify a bare word into its most specific [`Val`] variant, by the
    /// shape rules of [`crate::syntax::ast::WordLiteral::classify`].
    ///
    /// Eager and type-blind: a numeric-looking word meant as argv data is
    /// read as a number, and stringifies back unchanged only where its
    /// source was already canonical (`007` ⇒ `7`, `1.50` ⇒ `1.5`).
    pub fn from_word(s: &str) -> Self {
        use crate::syntax::ast::WordLiteral;
        match WordLiteral::classify(s) {
            Some(WordLiteral::Bool(b)) => Self::Bool(b),
            Some(WordLiteral::Unit) => Self::Unit,
            Some(WordLiteral::Int(n)) => Self::Int(n),
            Some(WordLiteral::Float(f)) => Self::Float(f),
            None => Self::String(s.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValListElem {
    Single(Val),
    /// `...x`, spliced into the surrounding list.
    Spread(Val),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValMapEntry {
    Entry(Val, Val),
    /// `...x`, merged into the surrounding map.
    Spread(Val),
}

/// Positional arguments to a call ([`CompKind::App`] or [`CompKind::Exec`]).
///
/// Each span covers a whole argument slot, the `...` of a spread included,
/// so a unification failure underlines one argument, not the whole call.
pub type Args = Vec<Spanned<ValListElem>>;

/// Readers of an [`Args`].  Free functions, not methods — [`Args`] is a
/// type alias.
pub mod args {
    use super::{Args, Val, ValListElem};

    /// Every sub-value in argument position, `Single` and `Spread` alike,
    /// for the inference passes that type sub-expressions best-effort.
    pub fn iter_subvals(args: &Args) -> impl Iterator<Item = &Val> {
        args.iter().map(|e| match &e.item {
            ValListElem::Single(v) | ValListElem::Spread(v) => v,
        })
    }

    /// The args as a literal positional list, or `None` if any element is a
    /// `Spread` — dynamic arity, so callers fall back to weaker checks.
    pub fn positional(args: &Args) -> Option<Vec<&Val>> {
        let mut out = Vec::with_capacity(args.len());
        for e in args {
            match &e.item {
                ValListElem::Single(v) => out.push(v),
                ValListElem::Spread(_) => return None,
            }
        }
        Some(out)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValRedirectTarget {
    File(Val),
    Fd(u32),
}

/// An I/O redirect.  Always owned by whatever it applies to — [`Exec`] or
/// [`ScopeOp::Redirect`] — never a wrapper of its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedirectV {
    pub fd: u32,
    pub mode: RedirectMode,
    pub target: ValRedirectTarget,
}

// ── Computations ────────────────────────────────────────────────────────

/// A computation with the source span elaboration gave it — the type the
/// evaluator interprets.
pub type Comp = Spanned<CompKind>;

/// True if this computation is a single external/builtin command call —
/// a fact about the input's shape, which hosts read to tailor what they say
/// about a failure.
pub fn is_single_command(comp: &Comp) -> bool {
    match &comp.item {
        CompKind::Exec(_) => true,
        CompKind::Seq(stmts) => {
            let mut commands = stmts.iter();
            matches!(commands.next().map(|c| &c.item), Some(CompKind::Exec(_)))
                && commands.next().is_none()
        }
        _ => false,
    }
}

/// Every name `comp` can reference, for the binding-lease ledger in
/// `core/src/types/shell/bindings.rs` to renew.  No wildcard arm anywhere
/// in the walk, so a new `CompKind` or `Val` variant is a compile error
/// here rather than a silently unharvested reference.  Over-approximate by
/// design: a name in an untaken branch renews too, and a lease only ever
/// lengthens.
pub(crate) fn referenced_names(comp: &Comp) -> Vec<&str> {
    let mut out = Vec::new();
    walk_comp(comp, &mut out);
    out
}

fn walk_comp<'a>(comp: &'a Comp, out: &mut Vec<&'a str>) {
    match &comp.item {
        CompKind::Lam { param, body } => {
            walk_pattern_defaults(param, out);
            walk_comp(body, out);
        }
        CompKind::Bind {
            comp,
            pattern,
            rest,
            scheme: _,
        } => {
            walk_comp(comp, out);
            walk_pattern_defaults(pattern, out);
            walk_comp(rest, out);
        }
        CompKind::App { head, args } => {
            walk_comp(head, out);
            walk_args(args, out);
        }
        CompKind::Exec(exec) => {
            walk_command_word(&exec.head, out);
            walk_args(&exec.args, out);
            walk_redirects(&exec.redirects, out);
        }
        CompKind::Pipeline {
            stages,
            stage_types: _,
            yields: _,
        } => {
            for stage in stages {
                walk_comp(stage, out);
            }
        }
        CompKind::Binary(_op, a, b) => {
            walk_val(a, out);
            walk_val(b, out);
        }
        CompKind::Force(v) | CompKind::Return(v) | CompKind::Negate(v) | CompKind::Not(v) => {
            walk_val(v, out);
        }
        CompKind::Index { target, keys } => {
            walk_val(target, out);
            for key in keys {
                walk_val(&key.item, out);
            }
        }
        CompKind::Chain(comps) | CompKind::Seq(comps) => {
            for c in comps {
                walk_comp(c, out);
            }
        }
        CompKind::Interpolation(vals) => {
            for v in vals {
                walk_val(v, out);
            }
        }
        CompKind::LetRec { slot: _, bindings } => {
            for (_name, rhs) in bindings.iter() {
                walk_val(rhs, out);
            }
        }
        CompKind::If { cond, then, else_ } => {
            walk_val(&cond.item, out);
            walk_comp(then, out);
            walk_comp(else_, out);
        }
        CompKind::Case { scrutinee, table } => {
            walk_val(&scrutinee.item, out);
            walk_val(&table.item, out);
        }
        CompKind::Scope(op) => walk_scope_op(op, out),
        CompKind::Capture(body) => walk_comp(body, out),
    }
}

fn walk_val<'a>(val: &'a Val, out: &mut Vec<&'a str>) {
    match val {
        Val::Unit
        | Val::String(_)
        | Val::Int(_)
        | Val::Float(_)
        | Val::Bool(_)
        | Val::TildePath(_) => {}
        Val::Variable(name) => out.push(name),
        Val::Thunk(comp) => walk_comp(comp, out),
        Val::List(elems) => {
            for elem in elems {
                match elem {
                    ValListElem::Single(v) | ValListElem::Spread(v) => walk_val(v, out),
                }
            }
        }
        Val::Map(entries) => {
            for entry in entries {
                match entry {
                    ValMapEntry::Entry(k, v) => {
                        walk_val(k, out);
                        walk_val(v, out);
                    }
                    ValMapEntry::Spread(v) => walk_val(v, out),
                }
            }
        }
        Val::Variant { label: _, payload } => {
            if let Some(p) = payload {
                walk_val(p, out);
            }
        }
    }
}

fn walk_args<'a>(args: &'a Args, out: &mut Vec<&'a str>) {
    for spanned in args {
        match &spanned.item {
            ValListElem::Single(v) | ValListElem::Spread(v) => walk_val(v, out),
        }
    }
}

/// Both dispatch forms contribute their head name.  A `^name` head can
/// never reach a binding, so collecting it over-approximates — the same
/// safe direction as an untaken branch.
fn walk_command_word<'a>(word: &'a CommandWord, out: &mut Vec<&'a str>) {
    if let CommandName::Bare(name) = word.name() {
        out.push(name);
    }
}

fn walk_redirects<'a>(redirects: &'a [RedirectV], out: &mut Vec<&'a str>) {
    for redirect in redirects {
        match &redirect.target {
            ValRedirectTarget::File(v) => walk_val(v, out),
            ValRedirectTarget::Fd(_) => {}
        }
    }
}

/// Map-pattern defaults are the only sub-position of an [`IrPattern`] that
/// can reference a name; the pattern's own names are bound, not referenced.
/// Recurses so a nested destructuring's defaults are found too.
fn walk_pattern_defaults<'a>(pattern: &'a IrPattern, out: &mut Vec<&'a str>) {
    match pattern {
        IrPattern::Wildcard | IrPattern::Name(_) => {}
        IrPattern::List { elems, rest: _ } => {
            for elem in elems {
                walk_pattern_defaults(elem, out);
            }
        }
        IrPattern::Map(entries) => {
            for entry in entries {
                walk_pattern_defaults(&entry.pattern, out);
                if let Some(default) = &entry.default {
                    walk_comp(default, out);
                }
            }
        }
    }
}

fn walk_scope_op<'a>(op: &'a ScopeOp, out: &mut Vec<&'a str>) {
    match op {
        ScopeOp::Try { body, handler } => {
            walk_val(body, out);
            walk_val(handler, out);
        }
        ScopeOp::Guard { body, cleanup } => {
            walk_val(body, out);
            walk_val(cleanup, out);
        }
        ScopeOp::Within { opts, body } => {
            walk_val(opts, out);
            walk_val(body, out);
        }
        ScopeOp::Grant { caps, body } => {
            walk_val(caps, out);
            walk_val(body, out);
        }
        ScopeOp::Audit { body } => walk_val(body, out),
        ScopeOp::Redirect { body, redirects } => {
            walk_comp(body, out);
            walk_redirects(redirects, out);
        }
    }
}

/// What a [`CompKind::Pipeline`] returns to whoever ran it: the last stage's
/// reported value, or unit because that stage's payload stayed on the byte
/// channel and so never crossed the process boundary.
///
/// Elaboration decides this and writes it down; nothing downstream re-derives
/// it from a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipeYield {
    Last,
    Unit,
}

/// The CBPV computation category — the evaluator steps a program by
/// matching on this.  Variant docs use Levy's CBPV notation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CompKind {
    /// force V — run a thunk.
    Force(Val),
    /// λ — evaluates to a closure.
    Lam { param: Param, body: Arc<Comp> },
    /// return V — produce a value.
    Return(Val),
    /// M to x. N — run `comp`, bind its result, continue with `rest`.
    Bind {
        comp: Arc<Comp>,
        pattern: IrPattern,
        rest: Arc<Comp>,
        /// Written by the annotation pass for a top-level `Bind` over a
        /// `Name` pattern, and installed beside the value so the next run's
        /// check starts from the live binding.  Closed — every variable
        /// ground or quantified — so it survives across per-run unifiers.
        scheme: Option<Box<crate::typecheck::Scheme>>,
    },
    /// `M : A → B, V : A ⊢ M V : B` — the elimination form taken when the
    /// head resolves to a bound value (`$f x`, `(|x| body) x`).  It carries
    /// no redirects, those being a shell effect and not a property of
    /// application; trailing ones become a [`ScopeOp::Redirect`] around it.
    App { head: Arc<Comp>, args: Args },
    /// Shell command invocation, and the effect boundary — nothing outside
    /// this variant reaches the dispatch chain or an external program.
    /// Redirects fuse into the call rather than wrapping it, because the
    /// spawn syscall installs descriptors and execs atomically.
    Exec(Exec),
    /// Concurrent stages joined by Unix pipes: stdout of stage N feeds
    /// stdin of stage N+1.
    Pipeline {
        stages: Vec<Arc<Comp>>,
        /// The inferred value type out of each stage, parallel to `stages`.
        /// Only the structural REPL's typed spine reads it, so an
        /// un-annotated pipeline keeps the `Unit` placeholder harmlessly.
        stage_types: Vec<crate::typecheck::Ty>,
        /// What the pipeline hands back.  Every interior edge is an operating-
        /// system byte pipe allocated from stage position alone, so this is
        /// the whole of the form's value behaviour.
        yields: PipeYield,
    },
    /// Binary primitive on already-evaluated values (`$[a + b]`, `$[a == b]`).
    Binary(BinaryOp, Val, Val),
    /// `-v` on a number.  Its own variant rather than a subtraction from a
    /// literal zero, which would have to pick that zero's type and so force
    /// the operand to match it — negating a `Float` would not typecheck.
    Negate(Val),
    /// `not v` on a `Bool`.  Its own variant so the IR cannot spell a
    /// two-operand `not` or a one-operand `Add`; evaluator and typechecker
    /// dispatch on the tag rather than a runtime arity guard.
    Not(Val),
    /// `V[k1][k2]` — computation-typed only because it can fail (key not
    /// found, out of bounds); target and keys are themselves pure.
    Index {
        target: Val,
        keys: Vec<Spanned<Val>>,
    },
    /// Fallback chain (`a ? b ? c`) — the first computation that succeeds.
    Chain(Vec<Arc<Comp>>),
    /// String interpolation, effectful because a lookup can fail.
    Interpolation(Vec<Val>),
    /// Sequence; the last value is the result.
    Seq(Vec<Arc<Comp>>),
    /// Simultaneous fixed point for mutual recursion.  Each RHS is a thunk,
    /// since the fixpoint must close over references to its siblings.
    /// `slot: None` establishes the whole group in the current shell and
    /// returns Unit; `Some(i)` re-establishes it in a temporary scope and
    /// returns the lambda for binding `i`.
    LetRec {
        slot: Option<usize>,
        bindings: Arc<Vec<(String, Val)>>,
    },
    /// `if V then M else N` with `V : Bool` and `M, N : C`; the chosen
    /// branch runs inline.
    If {
        cond: Spanned<Val>,
        then: Arc<Comp>,
        else_: Arc<Comp>,
    },
    /// Sum eliminator: `table` is a tag-keyed record of thunks, the matching
    /// one forced on the scrutinee's payload.  Literal handler tables are
    /// checked for exhaustiveness; an opaque table may still miss at
    /// runtime, which `evaluator::case` reports as an ordinary
    /// missing-handler error.
    Case {
        scrutinee: Spanned<Val>,
        table: Spanned<Val>,
    },
    /// Effect-frame scope: install an effect for the duration of a body,
    /// then restore.
    Scope(ScopeOp),
    /// Checker-inserted value boundary: run `body` with its byte channel
    /// captured, decode the bytes as its value. No surface syntax.
    Capture(Arc<Comp>),
}

/// Body of a [`CompKind::Exec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exec {
    pub head: CommandWord,
    /// Each element coerces to its string form at the syscall boundary.
    pub args: Args,
    pub redirects: Vec<RedirectV>,
}

/// Dispatch shape of an [`Exec`] head — a variant rather than a flag on
/// `Name`, so the IR shape carries the decision instead of burying it in a
/// boolean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandWord {
    /// Resolved at evaluation time: env, then handlers, then PATH.
    Name(CommandName),
    /// `^name` — skips the env, and so skips every native, but still
    /// resolves through handlers, so an enclosing `within [handlers:]` frame
    /// still contains it.  The bypass is on the lookup, not on the frame.
    External(CommandName),
}

impl CommandWord {
    /// The head name, common to both variants.
    pub fn name(&self) -> &CommandName {
        match self {
            Self::Name(n) | Self::External(n) => n,
        }
    }
}

/// The effect frames: each installs an effect for the duration of a body,
/// then restores it when the body returns or escapes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScopeOp {
    /// `try BODY HANDLER` — catch an error out of `body` and pass it to
    /// `handler`, a thunk of one argument.
    Try { body: Val, handler: Val },
    /// `guard BODY CLEANUP` — `cleanup` runs unconditionally; a failure in
    /// it is reported but does not mask the body's result.
    Guard { body: Val, cleanup: Val },
    /// `within OPTS BODY` — install the option overrides in `opts`, a map
    /// evaluated at runtime, for the duration of `body`.
    Within { opts: Val, body: Val },
    /// `grant CAPS BODY` — attenuate the active capability set across `body`.
    Grant { caps: Val, body: Val },
    /// `audit BODY` — record an audit subtree over `body` and reify it as a
    /// `[status, value, error, children]` record.
    Audit { body: Val },
    /// Redirect frame for a body that cannot fuse its own redirects — a
    /// CBPV `App`, or a nested `Scope`.  `body` is an `Arc<Comp>` and not a
    /// thunk-shaped `Val`, so the invoke arm needs no runtime fallback.
    Redirect {
        body: Arc<Comp>,
        redirects: Vec<RedirectV>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::tilde::TildePath;
    use crate::syntax::ast::{BinaryOp, RedirectMode};
    use crate::typecheck::Ty;

    #[test]
    fn from_word_classifies_canonical_numbers() {
        assert_eq!(Val::from_word("5"), Val::Int(5));
        assert_eq!(Val::from_word("42"), Val::Int(42));
        assert_eq!(Val::from_word("0"), Val::Int(0));
        assert_eq!(Val::from_word("2.5"), Val::Float(2.5));
        assert_eq!(Val::from_word("true"), Val::Bool(true));
        assert_eq!(Val::from_word("unit"), Val::Unit);
        assert_eq!(Val::from_word("hello"), Val::String("hello".into()));
    }

    // ── referenced_names: exhaustive walker coverage ─────────────────────

    fn var(name: &str) -> Val {
        Val::Variable(name.to_string())
    }

    fn ret(name: &str) -> Arc<Comp> {
        Arc::new(Spanned::synthetic(CompKind::Return(var(name))))
    }

    /// One synthetic `Comp` touching every `CompKind`, `Val`, and `ScopeOp`
    /// variant: `r_*` labels what it references, `*_bound` what it merely
    /// binds.  The harvest is asserted *exactly* — a subset would hide a
    /// wildcard-arm regression, a superset a bound name over-renewing.
    #[test]
    fn referenced_names_walks_every_variant() {
        let lam_param = IrPattern::Map(vec![crate::syntax::ast::MapPatternEntry {
            key: crate::syntax::ast::MapKey::Bare("p".into()),
            pattern: IrPattern::Name("lam_param_bound".into()),
            default: Some(Arc::new(Spanned::synthetic(CompKind::Return(var(
                "r_lam_default",
            ))))),
        }]);
        let lam = Spanned::synthetic(CompKind::Lam {
            param: lam_param,
            body: ret("r_lam_body"),
        });

        let bind_pattern = IrPattern::List {
            elems: vec![IrPattern::Map(vec![crate::syntax::ast::MapPatternEntry {
                key: crate::syntax::ast::MapKey::Bare("k".into()),
                pattern: IrPattern::Name("bind_map_bound".into()),
                default: Some(Arc::new(Spanned::synthetic(CompKind::Return(var(
                    "r_bind_pattern_default",
                ))))),
            }])],
            rest: Some("bind_rest_bound".into()),
        };
        let bind = Spanned::synthetic(CompKind::Bind {
            comp: ret("r_bind_comp"),
            pattern: bind_pattern,
            rest: ret("r_bind_rest"),
            scheme: None,
        });

        let app = Spanned::synthetic(CompKind::App {
            head: Arc::new(Spanned::synthetic(CompKind::Force(var("r_app_head")))),
            args: vec![
                Spanned::synthetic(ValListElem::Single(var("r_app_arg_single"))),
                Spanned::synthetic(ValListElem::Spread(var("r_app_arg_spread"))),
            ],
        });

        let exec_name = Spanned::synthetic(CompKind::Exec(Exec {
            head: CommandWord::Name(CommandName::Bare("r_exec_name_head".into())),
            args: vec![Spanned::synthetic(ValListElem::Single(var("r_exec_arg")))],
            redirects: vec![RedirectV {
                fd: 1,
                mode: RedirectMode::Write,
                target: ValRedirectTarget::File(var("r_exec_redirect_target")),
            }],
        }));
        let exec_external = Spanned::synthetic(CompKind::Exec(Exec {
            head: CommandWord::External(CommandName::Bare("r_exec_external_head".into())),
            args: vec![],
            redirects: vec![RedirectV {
                fd: 0,
                mode: RedirectMode::Read,
                // An `Fd` target contributes no reference to over-collect.
                target: ValRedirectTarget::Fd(9),
            }],
        }));

        let pipeline = Spanned::synthetic(CompKind::Pipeline {
            stages: vec![
                Arc::new(Spanned::synthetic(CompKind::Force(var(
                    "r_pipeline_stage1",
                )))),
                Arc::new(Spanned::synthetic(CompKind::Force(var(
                    "r_pipeline_stage2",
                )))),
            ],
            stage_types: vec![Ty::Unit, Ty::Unit],
            yields: PipeYield::Last,
        });

        let binary = Spanned::synthetic(CompKind::Binary(
            BinaryOp::Add,
            var("r_binary_a"),
            var("r_binary_b"),
        ));
        let not = Spanned::synthetic(CompKind::Not(var("r_not")));
        let index = Spanned::synthetic(CompKind::Index {
            target: var("r_index_target"),
            keys: vec![Spanned::synthetic(var("r_index_key"))],
        });
        let chain = Spanned::synthetic(CompKind::Chain(vec![ret("r_chain_a"), ret("r_chain_b")]));
        let interpolation = Spanned::synthetic(CompKind::Interpolation(vec![
            var("r_interp_a"),
            var("r_interp_b"),
        ]));
        let seq_inner = Spanned::synthetic(CompKind::Seq(vec![ret("r_seq_inner")]));
        let letrec = Spanned::synthetic(CompKind::LetRec {
            slot: None,
            bindings: Arc::new(vec![("letrec_name_bound".to_string(), var("r_letrec_rhs"))]),
        });
        let if_ = Spanned::synthetic(CompKind::If {
            cond: Spanned::synthetic(var("r_if_cond")),
            then: ret("r_if_then"),
            else_: ret("r_if_else"),
        });
        let case = Spanned::synthetic(CompKind::Case {
            scrutinee: Spanned::synthetic(var("r_case_scrutinee")),
            table: Spanned::synthetic(var("r_case_table")),
        });

        let scope_try = Spanned::synthetic(CompKind::Scope(ScopeOp::Try {
            body: var("r_try_body"),
            handler: var("r_try_handler"),
        }));
        let scope_guard = Spanned::synthetic(CompKind::Scope(ScopeOp::Guard {
            body: var("r_guard_body"),
            cleanup: var("r_guard_cleanup"),
        }));
        let scope_within = Spanned::synthetic(CompKind::Scope(ScopeOp::Within {
            opts: var("r_within_opts"),
            body: var("r_within_body"),
        }));
        let scope_grant = Spanned::synthetic(CompKind::Scope(ScopeOp::Grant {
            caps: var("r_grant_caps"),
            body: var("r_grant_body"),
        }));
        let scope_audit = Spanned::synthetic(CompKind::Scope(ScopeOp::Audit {
            body: var("r_audit_body"),
        }));
        let scope_redirect = Spanned::synthetic(CompKind::Scope(ScopeOp::Redirect {
            body: ret("r_scope_redirect_body"),
            redirects: vec![RedirectV {
                fd: 2,
                mode: RedirectMode::Append,
                target: ValRedirectTarget::File(var("r_scope_redirect_target")),
            }],
        }));

        let val_list = Spanned::synthetic(CompKind::Return(Val::List(vec![
            ValListElem::Single(Val::Unit),
            ValListElem::Single(Val::String("s".into())),
            ValListElem::Single(Val::Int(1)),
            ValListElem::Single(Val::Float(1.0)),
            ValListElem::Single(Val::Bool(true)),
            ValListElem::Single(Val::TildePath(TildePath {
                user: None,
                suffix: None,
            })),
            ValListElem::Single(var("r_list_single")),
            ValListElem::Spread(var("r_list_spread")),
        ])));
        let val_map = Spanned::synthetic(CompKind::Return(Val::Map(vec![
            ValMapEntry::Entry(var("r_map_key"), var("r_map_value")),
            ValMapEntry::Spread(var("r_map_spread")),
        ])));
        let val_variant = Spanned::synthetic(CompKind::Return(Val::Variant {
            label: "lbl".into(),
            payload: Some(Box::new(var("r_variant_payload"))),
        }));
        let val_variant_empty = Spanned::synthetic(CompKind::Return(Val::Variant {
            label: "lbl_empty".into(),
            payload: None,
        }));
        let val_thunk = Spanned::synthetic(CompKind::Return(Val::Thunk(Arc::new(
            Spanned::synthetic(CompKind::Return(var("r_thunk_body"))),
        ))));
        let capture = Spanned::synthetic(CompKind::Capture(ret("r_capture_body")));

        let whole = Spanned::synthetic(CompKind::Seq(vec![
            Arc::new(Spanned::synthetic(CompKind::Force(var("r_force")))),
            Arc::new(Spanned::synthetic(CompKind::Return(var("r_return")))),
            Arc::new(lam),
            Arc::new(bind),
            Arc::new(app),
            Arc::new(exec_name),
            Arc::new(exec_external),
            Arc::new(pipeline),
            Arc::new(binary),
            Arc::new(not),
            Arc::new(index),
            Arc::new(chain),
            Arc::new(interpolation),
            Arc::new(seq_inner),
            Arc::new(letrec),
            Arc::new(if_),
            Arc::new(case),
            Arc::new(scope_try),
            Arc::new(scope_guard),
            Arc::new(scope_within),
            Arc::new(scope_grant),
            Arc::new(scope_audit),
            Arc::new(scope_redirect),
            Arc::new(val_list),
            Arc::new(val_map),
            Arc::new(val_variant),
            Arc::new(val_variant_empty),
            Arc::new(val_thunk),
            Arc::new(capture),
        ]));

        let found: std::collections::HashSet<&str> = referenced_names(&whole).into_iter().collect();

        let expected = [
            "r_force",
            "r_return",
            "r_lam_default",
            "r_lam_body",
            "r_bind_pattern_default",
            "r_bind_comp",
            "r_bind_rest",
            "r_app_head",
            "r_app_arg_single",
            "r_app_arg_spread",
            "r_exec_name_head",
            "r_exec_arg",
            "r_exec_redirect_target",
            "r_exec_external_head",
            "r_pipeline_stage1",
            "r_pipeline_stage2",
            "r_binary_a",
            "r_binary_b",
            "r_not",
            "r_index_target",
            "r_index_key",
            "r_chain_a",
            "r_chain_b",
            "r_interp_a",
            "r_interp_b",
            "r_seq_inner",
            "r_letrec_rhs",
            "r_if_cond",
            "r_if_then",
            "r_if_else",
            "r_case_scrutinee",
            "r_case_table",
            "r_try_body",
            "r_try_handler",
            "r_guard_body",
            "r_guard_cleanup",
            "r_within_opts",
            "r_within_body",
            "r_grant_caps",
            "r_grant_body",
            "r_audit_body",
            "r_scope_redirect_body",
            "r_scope_redirect_target",
            "r_list_single",
            "r_list_spread",
            "r_map_key",
            "r_map_value",
            "r_map_spread",
            "r_variant_payload",
            "r_thunk_body",
            "r_capture_body",
        ];

        for name in expected {
            assert!(found.contains(name), "missing reference: {name}");
        }
        assert_eq!(
            found.len(),
            expected.len(),
            "unexpected extra name in {found:?}; every bound-not-referenced \
             name (lam_param_bound, bind_map_bound, bind_rest_bound, \
             letrec_name_bound) must be absent"
        );

        for bound in [
            "lam_param_bound",
            "bind_map_bound",
            "bind_rest_bound",
            "letrec_name_bound",
        ] {
            assert!(
                !found.contains(bound),
                "a bound (not referenced) name leaked into the harvest: {bound}"
            );
        }
    }
}
