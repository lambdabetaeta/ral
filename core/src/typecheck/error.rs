//! The type-error taxonomy: the structural causes the unifier and
//! inferencer raise, their constraint provenance, and the located error
//! they are packaged into.
//!
//! `TypeError` and `TypeErrorKind` represent the diagnostics produced by
//! unification and inference failures; `Reason` records *why* a constraint
//! was demanded, and `CompDiff` records which components of a computation
//! type disagreed.  Every user-facing sentence derived from these lives in
//! `explain.rs`.

use super::ty::{CompTy, PipeMode, Ty};
use crate::source::Span;
use crate::syntax::ast::BinaryOpKind;

/// A single component diff within a `CompTyMismatch` error.
///
/// When two computation types fail to unify, individual diffs record which
/// components (stdin mode, stdout mode, return type) disagreed and what their
/// resolved types were at the point of failure.
#[derive(Debug, Clone)]
pub enum CompDiff {
    Stdin {
        expected: PipeMode,
        actual: PipeMode,
    },
    Stdout {
        expected: PipeMode,
        actual: PipeMode,
    },
    ReturnType {
        expected: Ty,
        actual: Ty,
    },
}

/// The provenance of a constraint — *why* the inferencer demanded that
/// two types, computation types, or pipeline modes agree.
///
/// Carried on a constraint-failure [`TypeError`] (`reason: Some(..)`) and
/// absent on a direct diagnosis, which is already its own complete story
/// (`reason: None`).  `Reason` is data only — every sentence derived from
/// it lives in `explain.rs`, which is what makes each hint unit-testable
/// and reviewable in one place.
#[derive(Debug, Clone)]
pub enum Reason {
    /// A `[a, b, ...]` pattern's scrutinee against the list shape it destructures.
    ListPattern,
    /// A `[key: name, ...]` pattern's scrutinee against the record shape it destructures.
    RecordPattern,
    /// An applied argument's type against the function's parameter type.
    Argument,
    /// A call argument's type against an alias/handler arm's argv element type.
    AliasArgv,
    /// An alias/handler arm's declared parameter against the argv list shape.
    AliasParam,
    /// A builtin argument against the block/lambda shape it expects.
    BuiltinBlockArg,
    /// A builtin argument's actual type against its declared per-position type template.
    BuiltinTypedArg,
    /// A pipeline stage's produced value against the next stage's function parameter.
    PipedValue { step_stream: bool },
    /// Two adjacent pipeline stages' byte-channel modes at the edge between them.
    PipelineEdge,
    /// An unresolved computation forced into `Return` shape to read its value type and modes.
    ReturnShape,
    /// An alias/handler arm's pipeline modes against the head it reinterprets.
    HandlerModePin,
    /// An `if` condition's type against `Bool`.
    IfCond,
    /// The two branches of an `if` against each other's value type.
    IfBranches,
    /// The two outcomes of a `try` against each other's observed value.
    TryArms,
    /// A `try` handler's type against the one-argument function shape it must have.
    TryHandler,
    /// A scope form's body against the thunk shape every control wrapper expects.
    ScopeBody,
    /// A `case` arm handler's payload type against the scrutinee's payload at that tag.
    CaseArmPayload,
    /// A `case` scrutinee's type against the variant shape `case` requires.
    CaseScrutinee,
    /// A `case` handler table's type against the record-of-thunks shape `case` requires.
    CaseTable,
    /// A list literal's element type against the list's shared element type.
    ListElem,
    /// A list spread's operand against the list shape it must itself have.
    ListSpread,
    /// A dynamic map key against `String`.
    MapKey,
    /// A dynamic map entry's value against the map's shared element type.
    MapElem,
    /// A map spread's operand against the map shape it must itself have.
    MapSpread,
    /// An options/capability map entry's value against its schema-declared field type.
    OptionField { form: &'static str, key: String },
    /// The `!` operator's operand against the block/thunk shape it forces.
    ForceOperand,
    /// `not`'s operand against `Bool`.
    NotOperand,
    /// The two operands of a binary operator against each other.
    BinaryOperands(BinaryOpKind),
    /// A list index key against `Int`.
    ListIndexKey,
    /// A map index key against `String`.
    MapIndexKey,
    /// An indexing target against the record shape carrying the field being read.
    RecordFieldRead,
    /// An indexing target pinned to `List` or `Map` by its runtime-computed key's type.
    DynamicIndexTarget,
    /// A command head still unknown enough to be a thunk, pinned to `Thunk` so application can unfold it.
    AutoderefHead,
    /// The `_type` probe's threaded result against the argument's own type.
    TypeProbe,
    /// A `letrec` binding's inferred type against its own self-referential placeholder.
    LetRecSelf,
    /// The `from-lines` Step shape's recursive tail placeholder against its own closing value.
    LinesStepSelf,
}

/// The structural cause of a type error — raised by the unifier or inferencer,
/// enriched by `InferCtx` with source spans and rendered at the diagnostic layer.
#[derive(Debug, Clone)]
pub enum TypeErrorKind {
    RecursiveRow,
    /// Structural nesting exceeds the unifier's defensive recursion
    /// ceiling.  The co-inductive guard terminates every *cyclic*
    /// obligation; this is the belt-and-braces stop for a variable-free
    /// type nested past any plausible source program, turning a would-be
    /// stack overflow into a graceful type error.
    TypeTooDeep,
    TyMismatch {
        expected: Ty,
        actual: Ty,
    },
    CompTyMismatch {
        expected: CompTy,
        actual: CompTy,
        diffs: Vec<CompDiff>,
    },
    ModeMismatch {
        expected: PipeMode,
        actual: PipeMode,
    },
    RowExtraField {
        label: String,
    },
    RowMissingField {
        label: String,
    },
    /// Command head is a non-function value (e.g. a literal `String` in
    /// command position with arguments).  Reported under the same code as
    /// `CompTyMismatch` (T0011) — it is the same condition, framed in
    /// surface terms instead of as a `Cmd a vs a → b` mismatch.  The flag
    /// records that the head/args IR shape suggests a single
    /// double-quoted string split by an unescaped inner quote.
    CommandNotCallable {
        ty: Ty,
        split_string_suspect: bool,
    },
    /// `case` arms do not match the scrutinee row: either a label is
    /// missing (no handler for some variant constructor) or extraneous
    /// (a handler labelled with a constructor the scrutinee can never
    /// produce).  Both directions are surfaced in one diagnostic.
    CaseNotExhaustive {
        missing: Vec<String>,
        extra: Vec<String>,
    },
    /// `case` handler at `label` does not have the right shape — its
    /// payload type fails to unify with the scrutinee's payload type at
    /// that constructor, or it is not a function at all.
    CaseLabelTypeMismatch {
        label: String,
        expected: Ty,
        found: Ty,
    },
    /// `case` scrutinee is concretely not a variant — no tag to dispatch on.
    CaseOnNonVariant {
        ty: Ty,
    },
    /// A control operator (`within`, `try`, `guard`, `grant`, `audit`) named
    /// in value position instead of command position.
    ControlOperatorAsValue {
        name: String,
    },
    /// An alias/`within`-handler name used as a first-class value; handlers
    /// are only invocable in command position.
    HandlerNotFirstClass {
        name: String,
    },
    /// A builtin command's name used as a first-class value; builtins are
    /// only invocable in command position.
    BuiltinNotFirstClass {
        name: String,
    },
    /// An `alias`/handler install attempted to name a builtin, which owns
    /// its name outright.
    CannotRedefineBuiltin {
        name: String,
        verb: &'static str,
    },
    /// An `alias`/handler install named a lexical binding already in
    /// scope; bare lookup resolves to the binding first.
    HandlerShadowedByBinding {
        name: String,
    },
    /// A builtin call's argument count does not match its signature —
    /// exactly `expected`, or at most `expected` when `at_most`.
    BuiltinArity {
        expected: usize,
        got: usize,
        at_most: bool,
    },
    /// `fail [status: 0]` — a nonzero status is required so `fail` cannot
    /// masquerade as a clean exit.
    FailStatusZero,
    /// The elaborated IR for an `alias name { body }` statement does not
    /// have the expected shape.
    MalformedAlias {
        detail: &'static str,
    },
    /// The elaborated IR for an `unalias name` statement does not have
    /// the expected shape.
    MalformedUnalias {
        detail: &'static str,
    },
    /// Indexing directly into a block value (`Thunk`) instead of its
    /// forced result.
    IndexIntoThunk,
    /// A record-field read (`$v[field]`) on a value that is concretely
    /// not a record.
    FieldOnNonRecord {
        label: String,
        ty: Ty,
    },
    /// A runtime-computed index (`$v[$k]`) on a value that accepts no
    /// key at all.
    DynamicIndexOnScalar {
        ty: Ty,
    },
}

impl TypeErrorKind {
    /// Stable per-phase error code (`T####`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::RecursiveRow => "T0002",
            Self::TypeTooDeep => "T0003",
            Self::TyMismatch { .. } => "T0010",
            Self::CompTyMismatch { .. } | Self::CommandNotCallable { .. } => {
                "T0011"
            }
            Self::ModeMismatch { .. } => "T0012",
            Self::RowExtraField { .. } => "T0020",
            Self::RowMissingField { .. } => "T0021",
            Self::CaseNotExhaustive { .. } => "T0030",
            Self::CaseLabelTypeMismatch { .. } => "T0031",
            Self::CaseOnNonVariant { .. } => "T0032",
            Self::ControlOperatorAsValue { .. } => "T0040",
            Self::HandlerNotFirstClass { .. } => "T0041",
            Self::BuiltinNotFirstClass { .. } => "T0042",
            Self::CannotRedefineBuiltin { .. } => "T0043",
            Self::HandlerShadowedByBinding { .. } => "T0044",
            Self::BuiltinArity { .. } => "T0050",
            Self::FailStatusZero => "T0051",
            Self::MalformedAlias { .. } => "T0052",
            Self::MalformedUnalias { .. } => "T0053",
            Self::IndexIntoThunk => "T0060",
            Self::FieldOnNonRecord { .. } => "T0061",
            Self::DynamicIndexOnScalar { .. } => "T0062",
        }
    }
}

/// A located type error: source span, structural cause, and optional
/// constraint provenance.
#[derive(Debug, Clone)]
pub struct TypeError {
    pub pos: Option<Span>,
    pub kind: TypeErrorKind,
    pub reason: Option<Reason>,
}

impl TypeError {
    /// Render the optional guidance sentence from the error's kind and
    /// provenance; all prose lives in `explain.rs`.
    pub fn hint(&self) -> Option<String> {
        super::explain::hint(&self.kind, self.reason.as_ref())
    }
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = self.kind.render_message();
        match self.pos {
            Some(sp) => write!(f, "@{}..{}: {}", sp.start, sp.end, msg),
            None => write!(f, "{msg}"),
        }
    }
}
