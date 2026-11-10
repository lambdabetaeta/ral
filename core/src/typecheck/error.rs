//! The type errors the checker raises: a structural cause, the provenance of
//! the failed constraint, and a span.  Their user-facing prose is in `explain.rs`.

use super::contract::Vocab;
use crate::ir::BinaryOp;
use crate::source::Span;
use crate::ty::{CompTy, Kind, Label, RefusedArg, Row, Ty};

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

/// A failed constraint with the reason it was raised under; boxed, being large.
pub(super) type Clash = Box<(TypeErrorKind, Reason)>;

/// A builtin still waiting for arguments where a ready computation is needed.
#[derive(Debug, Clone)]
pub struct UnderAppliedBuiltin {
    pub name: String,
    pub expected: usize,
    pub got: usize,
}

/// Why the inferencer demanded that two types agree.
///
/// Present exactly on constraint failures; a direct diagnosis stands alone
/// with `reason: None`.  Data only — the sentence each turns into is
/// composed in `explain.rs`.
#[derive(Debug, Clone)]
pub enum Reason {
    /// A right-hand side against the value its binding receives.
    Binding {
        /// Where closing the enclosing definition chose the default, if it did.
        defaulted_at: Option<Span>,
    },
    /// A call's head against its next argument and result.
    Call {
        /// Where closing the enclosing definition chose the default, if it did.
        defaulted_at: Option<Span>,
        /// The callee names a member of the recursive group being checked.
        recursive_call: bool,
    },
    /// A call's head against the thunk shape it must have.
    Head {
        name: Option<String>,
    },
    ListPattern,
    RecordPattern,
    Argument,
    /// A named arm against the one parameter its slot hands it, the argv.
    AliasParam,
    /// The catch-all against its two parameters, the name and the argv.
    CatchAllParams,
    /// A pipeline stage forced to `Return` shape: a stage still waiting for an
    /// argument is not a computation that can run.
    PipelineStageShape {
        builtin: Option<UnderAppliedBuiltin>,
    },
    /// A discarded statement forced to `Return` shape: its value is thrown
    /// away, so it must be ready to run, not a `Fun` still waiting for an
    /// argument — [`PipelineStageShape`](Self::PipelineStageShape)'s sibling
    /// for a non-tail `Seq` part and the program's own value.
    DiscardedValueShape {
        builtin: Option<UnderAppliedBuiltin>,
    },
    /// A redirect scopes a ready computation, not a future application.
    RedirectShape,
    /// An unresolved computation forced to `Return` shape to read its value.
    ReturnShape,
    /// A checker-inserted protection node re-inferred against its contract.
    ProtectContract,
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
    /// A `spawn`'s title against `String`.
    SpawnTitle,
    /// The handler an arm names, against the function of the payload it must be.
    CaseArmHandler,
    /// The `case` arms against one another, where exactly one of them runs.
    CaseArms,
    CaseScrutinee,
    /// The scrutinee's solved row against the labels the arms cover.
    CaseCoverage {
        scrutinee: Row,
        arms: Vec<Label>,
    },
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
    /// A block a contract file returns where its table admits a factory,
    /// against one parameter, the options, making the table's row.
    Factory,
    /// A written option's value against the closed set of tags a vocabulary names;
    /// the innermost one the clash met.
    Vocabulary(&'static Vocab),
    /// An arm that arrived as a value, against the block an arm is.
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
    MapIndexKey,
    RecordFieldRead,
    ListPositionRead,
    /// A computed key reads a map.
    MapKeyRead,
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

/// How a refused binding closes on itself without crossing data.
/// An argument path stays an argument path through nested thunks.
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
/// The solver attaches the span; `diagnostic.rs` renders it.
#[derive(Debug, Clone)]
pub enum TypeErrorKind {
    /// A binding runs, and a call applies, one computation nothing else decides.
    ConflictingDefaults {
        binding: Option<Span>,
        call: Option<Span>,
        /// The variable the binding forces, when it forces one.
        forced: Option<String>,
        /// The call's head, when it is a variable.
        callee: Option<String>,
    },
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
    /// A label a closed row lacks; the label's alphabet says whether the row
    /// is a record's or a variant's.
    RowExtraField {
        label: Label,
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
    /// A grant's nested table (`exec:`, `fs:`) given something that is no record.
    NotATable {
        key: &'static str,
        /// The table's first key, which its empty spelling is written with.
        first: &'static str,
        found: Ty,
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
        /// The longest bound prefix of `name` that ends before a `-`.
        boundary: Option<String>,
    },
    /// A binding of data run as a program: a bare head, `$name args`, or `!$name`.
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
    /// `shell::exec::command::check_argv` still refuses at the spawn.
    ExecArgNotText {
        command: String,
        ty: Ty,
        refused: RefusedArg,
    },
    /// The elaborated IR for an `alias name { body }` statement has the wrong shape.
    MalformedAlias {
        detail: &'static str,
    },
    /// The elaborated IR for an `unalias name` statement has the wrong shape.
    MalformedUnalias {
        detail: &'static str,
    },
    /// An index on a literal, `'x'[f]`: it binds to that word alone, so this
    /// is `cmd 'x'[f]` meant as `!{cmd 'x'}[f]`.  Both spelled as written.
    IndexOnLiteral {
        literal: String,
        key: String,
    },
    /// A pipe edge into a stage whose own root binds standard input.  The feed
    /// answers every read the stage makes, for the stage's whole run, so the
    /// producer on the other side of the `|` works for nobody.
    DeadPipeEdge {
        feed: StdinFeed,
    },
}

impl TypeErrorKind {
    /// Translate a failed equation using only its solved types and provenance.
    pub fn reported(self, reason: &Reason) -> Self {
        match (reason, &self) {
            (Reason::Head { name }, Self::TyMismatch { expected, actual }) => {
                let ty = match (&**expected, &**actual) {
                    (Ty::Thunk(_), other) | (other, Ty::Thunk(_))
                        if !matches!(other, Ty::Thunk(_)) =>
                    {
                        other.clone()
                    }
                    _ => return self,
                };
                match name {
                    Some(name) => Self::HeadBoundToValue {
                        name: name.clone(),
                        ty,
                    },
                    None => Self::CommandNotFunction { ty },
                }
            }
            (Reason::CaseScrutinee, Self::TyMismatch { expected, actual }) => {
                let ty = match (&**expected, &**actual) {
                    (Ty::Variant(_), other) | (other, Ty::Variant(_))
                        if !matches!(other, Ty::Variant(_)) =>
                    {
                        other.clone()
                    }
                    _ => return self,
                };
                Self::CaseOnNonVariant { ty }
            }
            (
                Reason::CaseCoverage { scrutinee, arms },
                Self::RowExtraField { .. } | Self::RowMissingField { .. },
            ) => {
                let produced: Vec<_> = scrutinee.fields().map(|(label, _)| label).collect();
                let missing: Vec<_> = produced
                    .iter()
                    .copied()
                    .filter(|label| !arms.contains(label))
                    .map(ToString::to_string)
                    .collect();
                let extra: Vec<_> = if scrutinee.is_open() {
                    Vec::new()
                } else {
                    arms.iter()
                        .filter(|label| !produced.contains(label))
                        .map(ToString::to_string)
                        .collect()
                };
                if missing.is_empty() && extra.is_empty() {
                    self
                } else {
                    Self::CaseNotExhaustive { missing, extra }
                }
            }
            (
                Reason::PipelineStageShape {
                    builtin: Some(builtin),
                }
                | Reason::DiscardedValueShape {
                    builtin: Some(builtin),
                },
                Self::CompTyMismatch { expected, actual },
            ) if matches!(
                (expected, actual),
                (CompTy::Fun(..), CompTy::Return(..)) | (CompTy::Return(..), CompTy::Fun(..))
            ) =>
            {
                Self::BuiltinArity {
                    name: builtin.name.clone(),
                    expected: builtin.expected,
                    got: builtin.got,
                }
            }
            _ => self,
        }
    }

    /// Stable per-phase error code (`T####`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConflictingDefaults { .. } => "T0080",
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
            Self::NotATable { .. } => "T0079",
            Self::CaseNotExhaustive { .. } => "T0030",
            Self::CaseOnNonVariant { .. } => "T0032",
            Self::ControlOperatorAsValue { .. } => "T0040",
            Self::HandlerNotFirstClass { .. } => "T0041",
            Self::BuiltinNotFirstClass { .. } => "T0042",
            Self::BuiltinArity { .. } => "T0050",
            Self::MalformedAlias { .. } => "T0052",
            Self::MalformedUnalias { .. } => "T0053",
            Self::DecoderTakesNoArgument { .. } => "T0054",
            Self::SpreadIntoApplication { .. } => "T0056",
            Self::ExecArgNotText { .. } => "T0057",
            Self::IndexOnLiteral { .. } => "T0063",
            Self::DeadPipeEdge { .. } => "T0070",
            Self::UnboundVariable { .. } => "T0071",
            Self::HeadBoundToValue { .. } => "T0072",
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
        let msg = self.message();
        match self.pos {
            Some(sp) => write!(f, "@{}..{}: {}", sp.start, sp.end, msg),
            None => write!(f, "{msg}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::{Grade, RowVar};

    fn mismatch(expected: Ty, actual: Ty) -> TypeErrorKind {
        TypeErrorKind::TyMismatch {
            expected: Box::new(expected),
            actual: Box::new(actual),
        }
    }

    #[test]
    fn reported_head_failures_name_bound_values_or_unnamed_heads() {
        let thunk = Ty::Thunk(Box::new(CompTy::pure(Ty::Unit)));
        for (expected, actual) in [(&thunk, &Ty::String), (&Ty::String, &thunk)] {
            for name in [Some("f".to_string()), None] {
                let reported = mismatch(expected.clone(), actual.clone())
                    .reported(&Reason::Head { name: name.clone() });
                match (name, reported) {
                    (Some(want), TypeErrorKind::HeadBoundToValue { name, ty }) => {
                        assert_eq!(name, want);
                        assert_eq!(ty, Ty::String);
                    }
                    (None, TypeErrorKind::CommandNotFunction { ty }) => {
                        assert_eq!(ty, Ty::String);
                    }
                    (_, other) => panic!("not a head diagnosis: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn reported_case_scrutinee_failures_name_the_non_variant() {
        let variant = Ty::Variant(Row::Empty);
        for (expected, actual) in [(&variant, &Ty::Int), (&Ty::Int, &variant)] {
            let reported =
                mismatch(expected.clone(), actual.clone()).reported(&Reason::CaseScrutinee);
            assert!(matches!(
                reported,
                TypeErrorKind::CaseOnNonVariant { ty: Ty::Int }
            ));
            assert_eq!(reported.code(), "T0032");
        }
    }

    #[test]
    fn reported_case_coverage_uses_both_label_sets_and_respects_open_rows() {
        let row = |tags: &[&str], tail| {
            tags.iter().rev().fold(tail, |rest, tag| {
                Row::Extend(
                    Label::Case((*tag).into()),
                    Box::new(Ty::Unit),
                    Box::new(rest),
                )
            })
        };
        let labels = |tags: &[&str]| -> Vec<Label> {
            tags.iter().map(|tag| Label::Case((*tag).into())).collect()
        };
        let strings = |tags: &[&str]| -> Vec<String> {
            labels(tags).iter().map(ToString::to_string).collect()
        };
        for (scrutinee, arms, missing, extra) in [
            (row(&["a", "b"], Row::Empty), vec!["a"], vec!["b"], vec![]),
            (row(&["a"], Row::Empty), vec!["a", "b"], vec![], vec!["b"]),
            (
                row(&["a"], Row::Empty),
                vec!["b", "c"],
                vec!["a"],
                vec!["b", "c"],
            ),
            (
                row(&["a", "b"], Row::Var(RowVar(0))),
                vec!["a", "c"],
                vec!["b"],
                vec![],
            ),
        ] {
            let reason = Reason::CaseCoverage {
                scrutinee,
                arms: labels(&arms),
            };
            for failure in [
                TypeErrorKind::RowMissingField {
                    label: "irrelevant".into(),
                },
                TypeErrorKind::RowExtraField {
                    label: Label::Case("irrelevant".into()),
                    known: vec![],
                },
            ] {
                let reported = failure.reported(&reason);
                assert_eq!(reported.code(), "T0030");
                let TypeErrorKind::CaseNotExhaustive {
                    missing: got_missing,
                    extra: got_extra,
                } = reported
                else {
                    panic!("not a coverage diagnosis: {reported:?}");
                };
                assert_eq!(got_missing, strings(&missing));
                assert_eq!(got_extra, strings(&extra));
            }
        }
        for (scrutinee, arms) in [
            (row(&["a", "b"], Row::Empty), vec!["a", "b"]),
            (row(&["a"], Row::Var(RowVar(0))), vec!["a", "b"]),
        ] {
            let reason = Reason::CaseCoverage {
                scrutinee,
                arms: labels(&arms),
            };
            assert!(matches!(
                TypeErrorKind::RowMissingField { label: "unchanged".into() }.reported(&reason),
                TypeErrorKind::RowMissingField { label } if label == "unchanged"
            ));
        }
    }

    #[test]
    fn reported_shape_failures_name_under_applied_builtins_only() {
        let producer = CompTy::pure(Ty::Unit);
        let function = CompTy::arrows([Ty::Int], producer.clone());
        for (expected, actual) in [(&function, &producer), (&producer, &function)] {
            for pipeline in [false, true] {
                for builtin in [false, true] {
                    let context = builtin.then(|| UnderAppliedBuiltin {
                        name: "add".into(),
                        expected: 2,
                        got: 1,
                    });
                    let reason = if pipeline {
                        Reason::PipelineStageShape { builtin: context }
                    } else {
                        Reason::DiscardedValueShape { builtin: context }
                    };
                    let reported = TypeErrorKind::CompTyMismatch {
                        expected: expected.clone(),
                        actual: actual.clone(),
                    }
                    .reported(&reason);
                    if builtin {
                        assert!(matches!(reported, TypeErrorKind::BuiltinArity {
                            name, expected: 2, got: 1,
                        } if name == "add"));
                    } else {
                        assert!(matches!(reported, TypeErrorKind::CompTyMismatch { .. }));
                    }
                }
            }
        }
    }

    #[test]
    fn reported_preserves_failures_that_are_not_the_reason_target() {
        let reasons = [
            Reason::Head {
                name: Some("f".into()),
            },
            Reason::Head { name: None },
            Reason::CaseScrutinee,
            Reason::CaseCoverage {
                scrutinee: Row::Empty,
                arms: vec![],
            },
            Reason::PipelineStageShape {
                builtin: Some(UnderAppliedBuiltin {
                    name: "add".into(),
                    expected: 2,
                    got: 1,
                }),
            },
            Reason::DiscardedValueShape {
                builtin: Some(UnderAppliedBuiltin {
                    name: "add".into(),
                    expected: 2,
                    got: 1,
                }),
            },
            Reason::Binding { defaulted_at: None },
            Reason::Call {
                defaulted_at: None,
                recursive_call: false,
            },
            Reason::Argument,
        ];
        for reason in reasons {
            assert!(matches!(
                mismatch(Ty::Int, Ty::String).reported(&reason),
                TypeErrorKind::TyMismatch { expected, actual }
                    if *expected == Ty::Int && *actual == Ty::String
            ));
            assert!(matches!(
                TypeErrorKind::CyclicType {
                    via: CycleVia::Applied
                }
                .reported(&reason),
                TypeErrorKind::CyclicType {
                    via: CycleVia::Applied
                }
            ));
            assert!(matches!(
                TypeErrorKind::CompTyMismatch {
                    expected: CompTy::pure(Ty::Int),
                    actual: CompTy::Return(Grade::Output, Box::new(Ty::Unit)),
                }
                .reported(&reason),
                TypeErrorKind::CompTyMismatch { .. }
            ));
        }
    }
    type Verdict<'a> = Option<(&'a [&'a str], &'a [&'a str])>;
    #[test]
    fn coverage_names_missing_arms_and_extra_arms_of_a_closed_row_only() {
        let row = |tags: &[&str], tail: Row| {
            tags.iter().rev().fold(tail, |rest, tag| {
                Row::Extend(
                    Label::Case((*tag).into()),
                    Box::new(Ty::Unit),
                    Box::new(rest),
                )
            })
        };
        let arms = |tags: &[&str]| -> Vec<Label> {
            tags.iter().map(|t| Label::Case((*t).into())).collect()
        };
        let strings =
            |tags: &[&str]| -> Vec<String> { arms(tags).iter().map(Label::to_string).collect() };
        let open = || Row::Var(crate::ty::RowVar(0));
        let cases: [(Row, &[&str], Verdict); 6] = [
            (row(&["a", "b"], Row::Empty), &["a", "b"], None),
            (row(&["a", "b"], Row::Empty), &["a"], Some((&["b"], &[]))),
            (row(&["a"], Row::Empty), &["a", "b"], Some((&[], &["b"]))),
            (
                row(&["a"], Row::Empty),
                &["b", "c"],
                Some((&["a"], &["b", "c"])),
            ),
            (row(&["a"], open()), &["a", "b"], None),
            (row(&["a", "b"], open()), &["a", "c"], Some((&["b"], &[]))),
        ];
        for (scrutinee, arm_tags, expected) in cases {
            let got = match (TypeErrorKind::RowMissingField {
                label: "witness".into(),
            })
            .reported(&Reason::CaseCoverage {
                scrutinee: scrutinee.clone(),
                arms: arms(arm_tags),
            }) {
                TypeErrorKind::CaseNotExhaustive { missing, extra } => Some((missing, extra)),
                TypeErrorKind::RowMissingField { .. } => None,
                other => panic!("not a coverage verdict: {other:?}"),
            };
            let want = expected.map(|(missing, extra)| (strings(missing), strings(extra)));
            assert_eq!(got, want, "{scrutinee:?} against {arm_tags:?}");
        }
    }
}
