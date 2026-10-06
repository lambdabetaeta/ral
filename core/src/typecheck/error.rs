//! The type errors the checker raises: a structural cause, the provenance of
//! the failed constraint, and a span.  Their user-facing prose is in `explain.rs`.

use crate::ir::BinaryOp;
use crate::source::Span;
use crate::ty::{CompTy, Kind, Ty};

/// What an arm stands in for, which decides what it must produce.
#[derive(Debug, Clone)]
pub enum Standing {
    /// A command the program does not define: the arm's value is its output.
    Command(String),
    /// A base frame ral itself provides: the arm returns what that returns.
    Own(String),
    /// The catch-all `handler:`, for every command in its extent.
    EveryCommand,
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
    /// An arm's declared parameter against the argv a call site hands it.
    AliasParam,
    /// A pipeline stage forced to `Return` shape: a stage still waiting for an
    /// argument is not a computation that can run.
    PipelineStageShape,
    /// A discarded statement forced to `Return` shape: its value is thrown
    /// away, so it must be ready to run, not a `Fun` still waiting for an
    /// argument — [`PipelineStageShape`](Self::PipelineStageShape)'s sibling
    /// for a non-tail `Seq` part and the program's own value.
    DiscardedValueShape,
    /// An unresolved computation forced to `Return` shape to read its value.
    ReturnShape,
    /// An arm against what it stands in for: a command's value is its output,
    /// a base frame returns what it returns.
    StandsIn(Standing),
    /// A non-final pipeline stage against `Unit`: a stage feeds the next by
    /// writing.  `stage` and `next` are the two heads, when they are named.
    PipelineStageWrites {
        stage: Option<String>,
        next: Option<String>,
    },
    IfCond,
    /// The `if` arms against one another.
    IfBranches,
    /// Shared by `try` and `?`, which elaborates to nested `try`.
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
    /// A `...x` inside a record literal, which updates fields; `base` is the
    /// spread's own name when it is a written variable.
    RecordUpdate {
        base: Option<String>,
    },
    /// A `...x` inside a map literal, which copies entries.
    MapSpread,
    /// A written option's value against the type the form declares at `key`.
    OptionField {
        form: &'static str,
        key: String,
    },
    /// A `handlers:` arm that arrived as a value, against the block an arm is.
    HandlerArm,
    /// The `!` operator's operand against the block shape it forces.
    ForceOperand,
    /// The coercion the checker itself inserted, `cap` and `decode`: no user
    /// sentence ever needs it.
    Capture,
    NotOperand,
    /// Unary `-`'s operand, which must be a number.
    Negation,
    /// One part of an interpolated string.
    Interpolation,
    BinaryOperands(BinaryOp),
    ListIndexKey,
    MapIndexKey,
    RecordFieldRead,
    /// A bare label on a target known to be a map, read as that key.
    MapKeyRead,
    /// An indexing target settled to `List` or `Map`, by its key or its kind.
    DynamicIndexTarget,
    /// A head still a bare variable, pinned to `Thunk` so application can unfold it.
    AutoderefHead,
    LetRecSelf,
}

/// Which stage-root redirect answers a stage's reads, and so leaves nothing
/// for the wire feeding it.  Only these two forms bind standard input; the
/// writing modes cannot appear here.
#[derive(Debug, Clone, Copy)]
pub enum StdinFeed {
    /// `< f` — a file supplies the stage's standard input.
    File,
    /// `<< w` — a word supplies it.
    HereString,
}

impl StdinFeed {
    /// How the user spelled it.
    pub(crate) fn spelling(self) -> &'static str {
        match self {
            Self::File => "<",
            Self::HereString => "<<",
        }
    }
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

/// How a refused binding would close on itself without crossing data: the
/// edge by which the search met the variable it started from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleVia {
    Applied,
    Argument,
    Returns,
}

/// What a [`TypeErrorKind::KindMismatch`] found wanting.
#[derive(Debug, Clone)]
pub enum KindFound {
    /// A type whose head the kind does not admit.
    Type(Box<Ty>),
    /// Another variable's kind, sharing no head with the required one.
    Used { kind: Kind, witness: Option<Span> },
}

/// The structural cause of a type error, raised by the unifier or inferencer.
/// `InferCtx` attaches the span; `diagnostic.rs` renders it.
#[derive(Debug, Clone)]
pub enum TypeErrorKind {
    RecursiveRow,
    /// Nesting past the unifier's depth ceiling — a stack-overflow guard.
    TypeTooDeep,
    /// A binding whose structure reaches its own variable along non-data
    /// edges alone: a recursive type must pass through a list, map, record or
    /// variant.
    CyclicType {
        via: CycleVia,
    },
    /// Boxed: two `Ty`s inline make this enum the largest thing every
    /// `Result` in the unifier carries, and a mismatch is the cold path.
    TyMismatch {
        expected: Box<Ty>,
        actual: Box<Ty>,
    },
    /// A variable's kind, imposed at `witness`, is not met by `found`.
    KindMismatch {
        found: KindFound,
        kind: Kind,
        witness: Option<Span>,
    },
    CompTyMismatch {
        expected: CompTy,
        actual: CompTy,
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
    /// The same label written twice in one record literal.  Every other
    /// precedence rule in a literal is first-wins; this one alone was
    /// last-wins, so it is refused rather than given a direction.
    DuplicateField {
        label: String,
    },
    /// A key the table knows and refuses: the advice it carries is the only
    /// thing the message has to say, so it is not folded into "unknown key".
    RefusedKey {
        form: &'static str,
        key: &'static str,
        advice: &'static str,
    },
    /// A key a contract file returns, or a form is written with, that its
    /// table does not name.
    UnknownKey {
        form: &'static str,
        key: String,
        offered: Vec<&'static str>,
    },
    /// A contract file returns something its table cannot ascribe: not a
    /// record, not `()`, and not the factory the table admits.
    ReturnNotRecord {
        form: &'static str,
        found: Ty,
        offered: Vec<&'static str>,
    },
    /// A non-function value in head position; shares T0011 with `CompTyMismatch`.
    CommandNotFunction {
        ty: Ty,
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
    /// `$name` names nothing in scope; `suggestions` are the nearest names
    /// that are.
    UnboundVariable {
        name: String,
        suggestions: Vec<String>,
    },
    /// A session binding of something other than a thunk in command position.
    HeadBoundToValue {
        name: String,
        ty: Ty,
    },
    /// Wrong argument count for `name`, whose signature declares `expected`.
    BuiltinArity {
        name: String,
        expected: usize,
        got: usize,
    },
    /// A `from-*` decoder reads the byte channel — no argument slot to fill.
    DecoderTakesNoArgument {
        name: String,
    },
    /// A decoder before a pipeline's last stage: it returns a value and
    /// writes nothing, so `next` is fed by nobody.
    DecoderMidPipeline {
        name: String,
        next: Option<String>,
    },
    /// `...` in the argument list of a value.  A spread is the notation of an
    /// argv, which only a command, an external, or a handler has; a value takes
    /// its arguments by application, at an arity its own type declares.
    SpreadIntoApplication {
        head: SpreadHead,
    },
    /// An argument whose type the exec boundary has no argument for.  An
    /// operating-system argument is one word, and `ty` is a shape with no single
    /// word to give — a list, a map, a block, a handle, bytes.
    ///
    /// Raised on a concrete type only: what a type variable hides here,
    /// `runtime::command::vet` still refuses at the spawn.
    ExecArgNotText {
        command: String,
        ty: Ty,
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
    /// An index on a literal, `'x'[f]`: it binds to that word alone, so this
    /// is `cmd 'x'[f]` meant as `!{cmd 'x'}[f]`.  Both spelled as written.
    IndexOnLiteral {
        literal: String,
        key: String,
    },
    /// A computed index (`$v[$k]`) on a value that accepts no key at all.
    DynamicIndexOnScalar {
        ty: Ty,
    },
    /// A computed index whose container nothing in the unit decides is a list
    /// or a map.  `names` is the target and key as written, when both are
    /// variables; `holder` is the binding whose value holds the index.
    IndexContainerUnknown {
        names: Option<(String, String)>,
        holder: Option<Span>,
    },
    /// A pipe edge into a stage whose own root binds standard input.  The feed
    /// answers every read the stage makes, for the stage's whole run, so the
    /// producer on the other side of the `|` works for nobody.
    DeadPipeEdge {
        feed: StdinFeed,
    },
}

impl TypeErrorKind {
    /// Stable per-phase error code (`T####`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::RecursiveRow => "T0002",
            Self::TypeTooDeep => "T0003",
            Self::CyclicType { .. } => "T0073",
            Self::KindMismatch { .. } => "T0074",
            Self::TyMismatch { .. } => "T0010",
            Self::CompTyMismatch { .. } | Self::CommandNotFunction { .. } => "T0011",
            Self::RowExtraField { .. } => "T0020",
            Self::RowMissingField { .. } => "T0021",
            Self::DuplicateField { .. } => "T0022",
            Self::RefusedKey { .. } => "T0026",
            Self::UnknownKey { .. } => "T0076",
            Self::ReturnNotRecord { .. } => "T0077",
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
            Self::SpreadIntoApplication { .. } => "T0056",
            Self::ExecArgNotText { .. } => "T0057",
            Self::IndexIntoThunk => "T0060",
            Self::FieldOnNonRecord { .. } => "T0061",
            Self::DynamicIndexOnScalar { .. } => "T0062",
            Self::IndexOnLiteral { .. } => "T0063",
            Self::DeadPipeEdge { .. } => "T0070",
            Self::UnboundVariable { .. } => "T0071",
            Self::HeadBoundToValue { .. } => "T0072",
            Self::IndexContainerUnknown { .. } => "T0075",
            Self::DecoderMidPipeline { .. } => "T0078",
        }
    }
}

/// A name that `let` bound to what `callee` returned, and `callee` returns `()`.
#[derive(Debug, Clone)]
pub(crate) struct UnitCall {
    pub(crate) name: String,
    pub(crate) callee: String,
}

/// A located type error: span, structural cause, and constraint provenance.
#[derive(Debug, Clone)]
pub struct TypeError {
    pub pos: Option<Span>,
    pub kind: TypeErrorKind,
    pub(crate) reason: Option<Reason>,
    /// The failed constraint met a weak variable, or the type one was fixed to.
    pub(crate) weak: Option<super::unify::WeakSource>,
    /// The failed constraint concerned a name a `let` bound to what a call returned,
    /// which was `()`.
    pub(crate) unit: Option<UnitCall>,
}

impl TypeError {
    /// The optional guidance sentence for this error, composed in `explain.rs`.
    pub fn hint(&self) -> Option<String> {
        super::explain::hint(
            &self.kind,
            self.reason.as_ref(),
            self.weak.as_ref(),
            self.unit.as_ref(),
        )
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
