//! The type errors the checker raises: a structural cause, the provenance of
//! the failed constraint, and a span.  Their user-facing prose is in `explain.rs`.

use super::ty::{CompTy, PipeMode, Ty};
use crate::source::Span;
use crate::syntax::ast::BinaryOpKind;

/// Which component of a computation type disagreed, within a `CompTyMismatch`.
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

/// Why the inferencer demanded that two types agree.  Present exactly on
/// constraint failures; a direct diagnosis stands alone with `reason: None`.
/// Data only — the sentence each turns into is composed in `explain.rs`.
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
    /// A stage's produced value against the next stage's parameter; the flag
    /// marks a lazy Step stream, which must be consumed explicitly.
    PipedValue {
        step_stream: bool,
    },
    /// The byte-channel modes of two adjacent stages, at the edge between them.
    PipelineEdge,
    /// An unresolved computation forced to `Return` shape to read its value and modes.
    ReturnShape,
    /// An arm's pipeline modes against those of the head it reinterprets.
    HandlerModePin,
    IfCond,
    IfBranches,
    TryArms,
    /// A `try` handler against the one-argument function shape it must have.
    TryHandler,
    /// A scope form's body against the thunk shape every control wrapper expects.
    ScopeBody,
    /// An arm handler's payload against the scrutinee's payload at that tag.
    CaseArmPayload,
    CaseScrutinee,
    /// A handler table against the record-of-thunks shape `case` requires.
    CaseTable,
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
    /// `_type`'s result threaded back to its argument, keeping the probe transparent.
    TypeProbe,
    LetRecSelf,
    /// `from-lines`' recursive Step tail against the stream it closes into.
    LinesStepSelf,
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
    /// A `case` handler at `label` is not a function, or its payload does not fit.
    CaseLabelTypeMismatch {
        label: String,
        expected: Ty,
        found: Ty,
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
    /// An install named a builtin, which owns its name outright.
    CannotRedefineBuiltin {
        name: String,
        verb: &'static str,
    },
    /// An install named a binding already in scope; bare lookup finds it first.
    HandlerShadowedByBinding {
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
    /// A nonzero status is required, so `fail` cannot masquerade as a clean exit.
    FailStatusZero,
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
            Self::DecoderTakesNoArgument { .. } => "T0054",
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
