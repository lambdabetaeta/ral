//! The type errors the checker raises: a structural cause, the provenance of
//! the failed constraint, and a span.  Their user-facing prose is in `explain.rs`.

use super::route::RouteMismatch;
use super::ty::{CompTy, PayloadRoute, Ty};
use crate::source::Span;
use crate::syntax::ast::BinaryOpKind;

/// Which component of a computation type disagreed, within a `CompTyMismatch`.
#[derive(Debug, Clone)]
pub enum CompDiff {
    Route {
        expected: PayloadRoute,
        actual: PayloadRoute,
    },
    ReturnType {
        expected: Ty,
        actual: Ty,
    },
}

/// Why an arm cannot be installed under `head`.
///
/// The two failures are genuinely different questions — one is about where a
/// payload lives, the other about what WF-2 then forces the returned value to
/// be — and the alias install, the handler `vet`, and the checker each render
/// them in their own words.
#[derive(Debug, Clone)]
pub enum PinFailure {
    /// The arm and the head disagree about where their payload lives.
    Route(RouteMismatch),
    /// The head is captured from stdout, so WF-2 makes the arm's value
    /// `Unit` — and this arm returns something else.
    ByteHeadReturnsValue(Ty),
}

/// Why the inferencer demanded that two types agree.
///
/// Present exactly on constraint failures; a direct diagnosis stands alone
/// with `reason: None`.  Data only — the sentence each turns into is
/// composed in `explain.rs`.
#[derive(Debug, Clone)]
pub enum Reason {
    ListPattern,
    RecordPattern,
    Argument,
    /// A call argument against the element type of the arm's argv list.
    AliasArgv,
    /// An arm's declared parameter against the argv list shape.
    AliasParam,
    BuiltinBlockArg,
    BuiltinTypedArg,
    /// A raising form's argument against the error-record shape it demands.
    ErrorRecordArg,
    /// A pipeline stage forced to `Return` shape: a stage still waiting for an
    /// argument is not a computation that can run.
    PipelineStageShape,
    /// An unresolved computation forced to `Return` shape to read its value and route.
    ReturnShape,
    /// An arm's payload route against that of the head it reinterprets.
    HandlerRoutePin,
    IfCond,
    IfBranches,
    ChainBranches,
    TryArms,
    /// A `try` handler against the one-argument function shape it must have.
    TryHandler,
    /// A scope form's body against the thunk shape every control wrapper expects.
    ScopeBody,
    /// An arm's bound payload against the scrutinee's payload at that tag.
    CaseArmPayload,
    /// The handler an arm names, against the function of the payload it must be.
    CaseArmHandler,
    /// The `case` arms against one another, where exactly one of them runs.
    CaseArms,
    CaseScrutinee,
    ListElem,
    ListSpread,
    MapKey,
    MapElem,
    MapSpread,
    /// An options-map entry against its schema-declared field type, under `form`.
    OptionField {
        form: &'static str,
        key: String,
    },
    /// The `!` operator's operand against the block shape it forces.
    ForceOperand,
    NotOperand,
    BinaryOperands(BinaryOpKind),
    ListIndexKey,
    MapIndexKey,
    RecordFieldRead,
    /// An indexing target pinned to `List` or `Map` by its computed key's type.
    DynamicIndexTarget,
    /// A head still a bare variable, pinned to `Thunk` so application can unfold it.
    AutoderefHead,
    LetRecSelf,
    /// `from-lines`' recursive Step tail against the stream it closes into.
    LinesStepSelf,
    /// An undecided payload route pinned at a value boundary (a `Bind` RHS, a
    /// join's byte or value side) so a later grounding becomes an honest
    /// mismatch rather than silent divergence.
    RoutePin,
}

/// What a refused spread was aimed at — as much of the head as the refusing
/// site can name, which is all the rewrite needs.
#[derive(Debug, Clone)]
pub enum SpreadHead {
    /// A builtin, whose signature declares both name and arity.
    Builtin { name: String, arity: usize },
    /// Any other applied head — a lambda, a parameter, a bound name — whose
    /// arity is not yet known, and need never be for the spread to be wrong.
    Applied,
}

/// The structural cause of a type error, raised by the unifier or inferencer.
/// `InferCtx` attaches the span; `diagnostic.rs` renders it.
#[derive(Debug, Clone)]
pub enum TypeErrorKind {
    RecursiveRow,
    /// Nesting past the unifier's depth ceiling — a stack-overflow guard.
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
    RouteMismatch {
        expected: PayloadRoute,
        actual: PayloadRoute,
    },
    RowExtraField {
        label: String,
        /// The labels the rejecting record does have, so the message can offer
        /// the alternatives and not only the miss.  Empty when neither side had
        /// concrete labels to name.
        known: Vec<String>,
    },
    RowMissingField {
        label: String,
    },
    /// A non-function value in head position; shares T0011 with `CompTyMismatch`.
    /// The flag marks a head/args shape suggesting a string split by a stray quote.
    CommandNotFunction {
        ty: Ty,
        split_string_suspect: bool,
    },
    /// `case` arms do not match the scrutinee row — missing and extra, together.
    CaseNotExhaustive {
        missing: Vec<String>,
        extra: Vec<String>,
    },
    /// `case` scrutinee is concretely not a variant — no tag to dispatch on.
    CaseOnNonVariant {
        ty: Ty,
    },
    /// `within`, `try`, `guard`, `grant`, or `audit` named in value position.
    ControlOperatorAsValue {
        name: String,
    },
    HandlerNotFirstClass {
        name: String,
    },
    BuiltinNotFirstClass {
        name: String,
    },
    /// Wrong argument count: exactly `expected`, or at most it when `at_most`.
    BuiltinArity {
        expected: usize,
        got: usize,
        at_most: bool,
    },
    /// A `from-*` decoder reads the byte channel — no argument slot to fill.
    DecoderTakesNoArgument {
        name: String,
    },
    /// `...` in the argument list of a value.  A spread is the notation of an
    /// argv, which only a command, an external, or a handler has; a value takes
    /// its arguments by application, at an arity its own type declares.
    SpreadIntoApplication {
        head: SpreadHead,
    },
    /// A nonzero status is required, so `fail` cannot masquerade as a clean exit.
    FailStatusZero,
    /// An error record's `message` is neither `String` nor `Bytes`: the one
    /// part of the shape a row cannot state, so the checker states it.
    ErrorRecordMessage {
        actual: Ty,
    },
    /// The elaborated IR for an `alias name { body }` statement has the wrong shape.
    MalformedAlias {
        detail: &'static str,
    },
    /// The elaborated IR for an `unalias name` statement has the wrong shape.
    MalformedUnalias {
        detail: &'static str,
    },
    /// Indexing into a block value instead of its forced result.
    IndexIntoThunk,
    /// A field read (`$v[field]`) on a value that is concretely not a record.
    FieldOnNonRecord {
        label: String,
        ty: Ty,
    },
    /// A computed index (`$v[$k]`) on a value that accepts no key at all.
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
            Self::CompTyMismatch { .. } | Self::CommandNotFunction { .. } => "T0011",
            Self::RouteMismatch { .. } => "T0012",
            Self::RowExtraField { .. } => "T0020",
            Self::RowMissingField { .. } => "T0021",
            Self::CaseNotExhaustive { .. } => "T0030",
            Self::CaseOnNonVariant { .. } => "T0032",
            Self::ControlOperatorAsValue { .. } => "T0040",
            Self::HandlerNotFirstClass { .. } => "T0041",
            Self::BuiltinNotFirstClass { .. } => "T0042",
            Self::BuiltinArity { .. } => "T0050",
            Self::FailStatusZero => "T0051",
            Self::MalformedAlias { .. } => "T0052",
            Self::MalformedUnalias { .. } => "T0053",
            Self::DecoderTakesNoArgument { .. } => "T0054",
            Self::ErrorRecordMessage { .. } => "T0055",
            Self::SpreadIntoApplication { .. } => "T0056",
            Self::IndexIntoThunk => "T0060",
            Self::FieldOnNonRecord { .. } => "T0061",
            Self::DynamicIndexOnScalar { .. } => "T0062",
        }
    }
}

/// A located type error: span, structural cause, and constraint provenance.
#[derive(Debug, Clone)]
pub struct TypeError {
    pub pos: Option<Span>,
    pub kind: TypeErrorKind,
    pub reason: Option<Reason>,
}

impl TypeError {
    /// The optional guidance sentence for this error, composed in `explain.rs`.
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
