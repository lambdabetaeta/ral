//! User-facing prose for type errors.  `infer.rs` and `unify.rs` raise them
//! as data; every sentence a user reads is a pure function of that data,
//! written here and nowhere else.

use super::contract::{Vocab, code, unknown_key_message};
use super::error::{
    CycleVia, KindFound, Reason, SpreadHead, Standing, TypeError, TypeErrorKind, UnitCall,
};
use super::unify::WeakSource;
use crate::diagnostic::{self, Report};
use crate::ir::{ArithOp, BinaryOp};
use crate::source::Span;
use crate::text::{indefinite, plural};
use crate::ty::{
    CompTy, FmtCtx, Grade, Kind, Label, RefusedArg, Row, TAG_PREFIX, Ty, fmt_comp_ty_ctx,
    fmt_ty_ctx,
};

impl TypeErrorKind {
    /// The headline sentence for this error.
    ///
    /// Symmetric by design: which side of a constraint lands in `expected` is
    /// an accident of the call site, so no message may claim one side is right.
    pub fn render_message(&self) -> String {
        match self {
            Self::ConflictingDefaults { .. } =>
                "the same computation is run for its value by a binding and called as a function by an application".into(),
            Self::RecursiveRow => {
                "infinite row: a record's field list would refer back to itself".into()
            }
            Self::TypeTooDeep => "type nesting exceeds the supported depth".into(),
            Self::CyclicType { via } => format!(
                "this makes a type that contains itself without going through any data: \
                 here a function {}",
                match via {
                    CycleVia::Applied => "is applied to itself",
                    CycleVia::Argument => "takes itself as an argument",
                    CycleVia::Returns => "returns itself",
                }
            ),
            Self::TyMismatch { expected, actual } => {
                let ctx = FmtCtx::for_value_types(&[expected, actual]);
                format!(
                    "couldn't match type {} with type {}",
                    fmt_ty_ctx(expected, &ctx),
                    fmt_ty_ctx(actual, &ctx)
                )
            }
            Self::KindMismatch { found, kind, .. } => match found {
                KindFound::Type(ty) => {
                    format!("this is {}, but {} is needed", describe(ty), needs(*kind))
                }
                KindFound::Used { kind: used, .. } => format!(
                    "this is used as {} and as {}: nothing is both",
                    role(*used),
                    role(*kind)
                ),
            },
            Self::CompTyMismatch { expected, actual } => fmt_comp_mismatch(expected, actual),
            Self::RowExtraField { label, .. } => match label {
                Label::Field(field) => format!("this record has no field named '{field}'"),
                tag @ Label::Case(_) => {
                    format!("this value cannot be the tag {}", code(&tag.to_string()))
                }
            },
            Self::RowMissingField { label } => {
                format!("this record is missing a field named '{label}'")
            }
            Self::DuplicateField { label } => {
                format!("this record literal writes the field '{label}' twice")
            }
            Self::RefusedKey { form, key, .. } => {
                format!("`{form}` does not take '{key}'")
            }
            Self::UnknownKey { form, key, offered } => unknown_key_message(form, key, offered),
            Self::ReturnNotRecord { form, found, .. } => format!(
                "`{form}` takes a record of settings, and this file returns {}",
                describe(found)
            ),
            Self::NotATable { key, found, .. } => format!(
                "`{key}` takes a record of settings, and this is {}",
                describe(found)
            ),
            Self::CommandNotFunction { ty } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!(
                    "value of type {} cannot be used as a command head",
                    fmt_ty_ctx(ty, &ctx)
                )
            }
            Self::CaseNotExhaustive { missing, extra } => fmt_case_exhaustiveness(missing, extra),
            Self::CaseOnNonVariant { ty } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!(
                    "`case` needs a variant value (something built with a backtick, like `` `ok 1 `` or `` `err msg ``), but this is a value of type {}",
                    fmt_ty_ctx(ty, &ctx)
                )
            }
            Self::ControlOperatorAsValue { name } => format!(
                "'{name}' is a control operator, not a value; it can only appear in command position"
            ),
            Self::HandlerNotFirstClass { name } => {
                format!("`{name}` is a handler entry, not a first-class value")
            }
            Self::BuiltinNotFirstClass { name } => {
                format!("`{name}` is a builtin command, not a first-class value")
            }
            Self::UnboundVariable { name, .. } => format!("undefined variable: ${name}"),
            Self::HeadBoundToValue { name, ty } => {
                format!(
                    "`{name}` is bound to {}, so it is not a program",
                    describe(ty)
                )
            }
            Self::BuiltinArity {
                name,
                expected,
                got,
            } => format!(
                "`{name}` expected {}, got {got}",
                plural(*expected, "argument")
            ),
            Self::DecoderTakesNoArgument { name } => {
                format!("`{name}` takes no argument: it reads the byte channel")
            }
            Self::DecoderMidPipeline { name, next } => format!(
                "a decoder ends the byte pipeline: `{name}` returns a value and writes \
                 nothing, so nothing reaches {}",
                next.as_ref()
                    .map_or_else(|| "the next stage".to_string(), |next| format!("`{next}`"))
            ),
            Self::SpreadIntoApplication { head } => {
                let takes = match head {
                    SpreadHead::Builtin { name, arity } => {
                        format!("`{name}` takes {}", plural(*arity, "argument"))
                    }
                    SpreadHead::Applied => "this head takes its arguments".into(),
                };
                format!(
                    "{takes} by application, and `...` spreads an argv: \
                     which only a command, an external, or a handler has"
                )
            }
            // The refusal `shell::exec::command::check_argv` raises at the
            // spawn, one step earlier.
            Self::ExecArgNotText { command, ty, .. } => {
                RefusedArg::headline(command, fmt_ty_ctx(ty, &FmtCtx::for_value_types(&[ty])))
            }
            Self::MalformedAlias { .. } => "malformed alias definition".into(),
            Self::MalformedUnalias { .. } => "malformed unalias".into(),
            Self::IndexOnLiteral { literal, key } => format!(
                "`[{key}]` reads from {literal} itself: an index attaches to the one word right before it"
            ),
            Self::DeadPipeEdge { feed } => format!(
                "the pipe writes into a stdin this `{}` replaces",
                feed.spelling()
            ),
        }
    }

    /// The bite-size pointer beside the source underline, next to the headline
    /// from [`render_message`](Self::render_message).  Both rebuild [`FmtCtx`]
    /// from the same types in the same order, so the `α` here is the `α` there.
    pub fn render_label(&self) -> String {
        match self {
            Self::ConflictingDefaults { .. } => "both run and called as a function".into(),
            Self::RecursiveRow => "the type loops back into itself here".into(),
            Self::TypeTooDeep => "the type nests too deeply here".into(),
            Self::CyclicType { .. } => "a type that contains itself".into(),
            Self::TyMismatch { expected, actual } => {
                let ctx = FmtCtx::for_value_types(&[expected, actual]);
                format!(
                    "{} doesn't match {}",
                    fmt_ty_ctx(expected, &ctx),
                    fmt_ty_ctx(actual, &ctx)
                )
            }
            Self::CompTyMismatch { .. } => "types disagree here".into(),
            Self::KindMismatch {
                found: KindFound::Type(ty),
                ..
            } => describe(ty),
            Self::KindMismatch { kind, .. } => format!("also used as {}", role(*kind)),
            Self::CommandNotFunction { ty } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!("{} cannot be invoked as a command", fmt_ty_ctx(ty, &ctx))
            }
            Self::RowExtraField { label, .. } => match label {
                Label::Field(field) => format!("no field '{field}' in this record"),
                tag @ Label::Case(_) => format!("no tag {tag} here"),
            },
            Self::RowMissingField { label } => format!("this record needs field '{label}'"),
            Self::DuplicateField { label } => format!("'{label}' was already given above"),
            Self::RefusedKey { key, .. } => format!("'{key}' is refused here"),
            Self::UnknownKey { key, .. } => format!("no such key: '{key}'"),
            Self::ReturnNotRecord { found, .. } => format!("returns {}", describe(found)),
            Self::NotATable { found, .. } => format!("is {}", describe(found)),
            Self::CaseNotExhaustive { missing, extra } => {
                match (missing.as_slice(), extra.as_slice()) {
                    ([only], []) => format!("no arm for {}", code(only)),
                    (some, []) => format!("no arm for {}", coded(some)),
                    ([], [only]) => format!("arm for {} that the value never produces", code(only)),
                    ([], some) => {
                        format!("arms for {} that the value never produces", coded(some))
                    }
                    _ => "case alternatives don't match the value".into(),
                }
            }
            Self::SpreadIntoApplication { .. } => "this spread has no argv to fill".into(),
            Self::ExecArgNotText { .. } => {
                "an external's arguments are words, and this is not one".into()
            }
            Self::DeadPipeEdge { .. } => "this stage reads here, not from the pipe".into(),
            Self::DecoderMidPipeline { name, .. } => {
                format!("`{name}` writes nothing to the pipe")
            }
            Self::CaseOnNonVariant { .. }
            | Self::ControlOperatorAsValue { .. }
            | Self::HandlerNotFirstClass { .. }
            | Self::BuiltinNotFirstClass { .. }
            | Self::BuiltinArity { .. }
            | Self::DecoderTakesNoArgument { .. }
            | Self::MalformedAlias { .. }
            | Self::MalformedUnalias { .. } => "here".into(),
            Self::UnboundVariable { .. } => "not defined".into(),
            Self::IndexOnLiteral { literal, .. } => {
                format!("this index belongs to {literal} alone")
            }
            Self::HeadBoundToValue { name, ty } => format!("this `{name}` is {}", describe(ty)),
        }
    }
}

impl TypeErrorKind {
    /// A related use, unless the error already stands there.
    pub(crate) fn witness(&self, at: Option<Span>) -> Option<(Span, String)> {
        if let Self::ConflictingDefaults { binding, call, .. } = self {
            return conflicting_uses(*binding, *call)
                .into_iter()
                .find_map(|(span, text)| {
                    Some((span.filter(|&span| Some(span) != at)?, text.into()))
                });
        }
        let Self::KindMismatch {
            found,
            kind,
            witness,
        } = self
        else {
            return None;
        };
        let cited = match found {
            KindFound::Type(_) => vec![(*witness, *kind)],
            KindFound::Used {
                kind: used,
                witness: earlier,
            } => vec![(*earlier, *used), (*witness, *kind)],
        };
        cited.into_iter().find_map(|(span, kind)| {
            let span = span.filter(|&s| Some(s) != at)?;
            Some((span, format!("used as {} here", role(kind))))
        })
    }
}

/// What a value of this type is, for a sentence.
fn describe(ty: &Ty) -> String {
    match ty {
        Ty::Unit => "nothing (`()`)",
        Ty::Bool => "a Bool",
        Ty::Int => "an Int",
        Ty::Float => "a Float",
        Ty::String => "text (String)",
        Ty::Bytes => "Bytes",
        Ty::List(_) => "a list",
        Ty::Map(_) => "a map",
        Ty::Record(_) => "a record",
        Ty::Variant(_) => "a tagged value",
        Ty::Thunk(_) => "a block",
        Ty::Handle(..) => "a handle",
        Ty::Var(_) => "a value of unknown type",
    }
    .into()
}

/// What a kind asks of a value.
fn needs(kind: Kind) -> String {
    match kind {
        Kind::NUMBER => "a number (Int or Float)".into(),
        Kind::COMPARABLE => "a number or text".into(),
        Kind::SCALAR => "text, a number or a Bool".into(),
        Kind::SIZED => "text, bytes, a list or a map".into(),
        Kind::DATA => "data (no blocks or handles)".into(),
        other => format!("something of kind `{other}`"),
    }
}

/// What a value is being used as, under a kind.
fn role(kind: Kind) -> String {
    match kind {
        Kind::NUMBER => "a number".into(),
        Kind::COMPARABLE => "something ordered with `<`".into(),
        Kind::SCALAR => "something interpolated into a string".into(),
        Kind::SIZED => "something `length` measures".into(),
        Kind::DATA => "data".into(),
        other => format!("something of kind `{other}`"),
    }
}

/// The help for a kind refusal: what the operator asks of its operands, then
/// the rewrite, both read off the reason and the type that was found.
fn kind_hint(found: &KindFound, required: Kind, reason: Option<&Reason>) -> Option<String> {
    let KindFound::Type(ty) = found else {
        return Some(
            "one value has one type: convert it at one of the two uses, or bind the \
             converted value to a new name"
                .to_string(),
        );
    };
    let lead = match reason {
        Some(Reason::BinaryOperands(BinaryOp::Arith(ArithOp::Mod))) => Some("`%` takes Ints"),
        Some(Reason::BinaryOperands(BinaryOp::Arith(_))) => {
            Some("`+ - * /` work on numbers (Int or Float)")
        }
        Some(Reason::Negation) => Some("`-` negates a number (Int or Float)"),
        Some(Reason::BinaryOperands(BinaryOp::Compare(_))) => Some("`<` compares numbers or text"),
        Some(Reason::BinaryOperands(BinaryOp::Eq(_))) => Some("`==` compares data"),
        Some(Reason::Interpolation) => {
            Some("only text, numbers and `true`/`false` go into a string")
        }
        _ => None,
    };
    let way = match (reason, &**ty) {
        (Some(Reason::BinaryOperands(BinaryOp::Arith(op))), Ty::String) if *op != ArithOp::Mod => {
            Some("to join text, interpolate: `\"$a$b\"`")
        }
        (_, Ty::List(_) | Ty::Record(_) | Ty::Map(_)) if required == Kind::COMPARABLE => {
            Some("sort with `sort-list-by` and a key")
        }
        (Some(Reason::Interpolation), Ty::List(_)) => {
            Some("join a list with `!{intercalate ', ' $xs}`")
        }
        (Some(Reason::Interpolation), Ty::Record(_) | Ty::Map(_)) => {
            Some("render it with `!{to-json $x}`")
        }
        (Some(Reason::Interpolation), Ty::Unit) => {
            Some("`()` is nothing to print; to print the text, write `'()'`")
        }
        (Some(Reason::Interpolation), Ty::Bytes) => {
            Some("decode it with `from-string`, or render it with `str`")
        }
        (_, Ty::Thunk(_)) if required == Kind::DATA => {
            Some("a block has no equality and no encoding: use what it computes, `!$f`")
        }
        (_, Ty::Handle(..)) if required == Kind::DATA => {
            Some("a handle has no equality and no encoding: use what `await` gives")
        }
        _ => None,
    };
    match (lead, way) {
        (Some(lead), Some(way)) => Some(format!("{lead}; {way}")),
        (one, other) => one.or(other).map(str::to_string),
    }
}

/// Prose for a `CompTyMismatch`.  A command against a value producer is
/// named as such; two producers of one kind disagree in what they return;
/// `unify.rs` blames no component when the two heads differ in shape —
/// `Return` against `Fun`.
fn fmt_comp_mismatch(expected: &CompTy, actual: &CompTy) -> String {
    match (expected, actual) {
        (CompTy::Return(g, expected), CompTy::Return(h, actual)) if g == h => {
            let ctx = FmtCtx::for_value_types(&[expected, actual]);
            format!(
                "couldn't match type {} with type {}",
                fmt_ty_ctx(expected, &ctx),
                fmt_ty_ctx(actual, &ctx)
            )
        }
        (CompTy::Return(..), CompTy::Return(..)) => {
            let ctx = FmtCtx::for_comp_types(&[expected, actual]);
            format!(
                "couldn't match type {} with type {}",
                fmt_comp_ty_ctx(expected, &ctx),
                fmt_comp_ty_ctx(actual, &ctx)
            )
        }
        _ => {
            "two computations have incompatible shapes: one is a function, the other is not".into()
        }
    }
}

/// The two value types a mismatch is between, as `(expected, actual)`: a
/// value mismatch's own, or what two producers of one kind return.
fn value_sides(kind: &TypeErrorKind) -> Option<(&Ty, &Ty)> {
    match kind {
        TypeErrorKind::TyMismatch { expected, actual } => Some((expected, actual)),
        TypeErrorKind::CompTyMismatch {
            expected: CompTy::Return(g, expected),
            actual: CompTy::Return(h, actual),
        } if g == h => Some((expected, actual)),
        _ => None,
    }
}

/// A command met a value producer: the value's type, and whether the command
/// was found where the value was demanded.
fn command_against_value(kind: &TypeErrorKind) -> Option<(&Ty, bool)> {
    match kind {
        TypeErrorKind::CompTyMismatch {
            expected: CompTy::Return(grade, value),
            actual: CompTy::Return(Grade::Output, _),
        } if *grade != Grade::Output => Some((value, true)),
        TypeErrorKind::CompTyMismatch {
            expected: CompTy::Return(Grade::Output, _),
            actual: CompTy::Return(grade, value),
        } if *grade != Grade::Output => Some((value, false)),
        _ => None,
    }
}

enum JoinForm {
    Branch,
    Arm,
    Outcome,
}

impl JoinForm {
    fn words(&self) -> (&'static str, &'static str) {
        match self {
            Self::Branch => ("branch", "a branch"),
            Self::Arm => ("arm", "an arm"),
            Self::Outcome => ("outcome", "an outcome"),
        }
    }
}

/// The hint where a command met a value producer, by what demanded which.
fn grade_hint(kind: &TypeErrorKind, reason: Option<&Reason>) -> Option<String> {
    let (value, command_found) = command_against_value(kind)?;
    let form = match reason {
        Some(Reason::IfBranches) => Some(JoinForm::Branch),
        Some(Reason::CaseArms) => Some(JoinForm::Arm),
        Some(Reason::TryArms) => Some(JoinForm::Outcome),
        Some(Reason::Argument) => None,
        // Every other reason says what it demanded in its own words.
        _ => return None,
    };
    Some(match (form.map(|form| form.words()), command_found, value) {
        (Some((form, phrase)), true, Ty::String | Ty::Var(_)) => format!(
            "this {form} is a command and an earlier one returns a value; {phrase}'s output is not captured: return it, `{{ return !{{…}} }}`"
        ),
        (Some((form, phrase)), false, Ty::String | Ty::Var(_)) => format!(
            "this {form} returns a value and an earlier one is a command; {phrase}'s output is not captured: make that one return it, `{{ return !{{…}} }}`"
        ),
        (Some((form, _)), true, Ty::Bool) => format!(
            "this {form} is a command and an earlier one returns a Bool; for a yes/no answer from a command, `succeeds {{ … }}`"
        ),
        (Some((form, _)), false, Ty::Bool) => format!(
            "this {form} returns a Bool and an earlier one is a command; for a yes/no answer from that command, `succeeds {{ … }}`"
        ),
        (Some((form, _)), true, _) => format!(
            "this {form} is a command and an earlier one returns {}; give this {form} a value of that type too (`… | from-json`, or `return` one), or write the earlier one's value out with `echo`",
            describe(value)
        ),
        (Some((form, _)), false, _) => format!(
            "this {form} returns {} and an earlier one is a command; give that one a value of that type too (`… | from-json`, or `return` one), or write this {form}'s value out with `echo`",
            describe(value)
        ),
        (None, true, Ty::String | Ty::Var(_)) =>
            "this block is a command, but a block that returns a value is wanted here: return its output, `{ |…| return !{…} }`".into(),
        (None, true, Ty::Bool) =>
            "this block is a command, but a block that returns a Bool is wanted here; for a yes/no answer, `succeeds { … }`".into(),
        (None, true, _) => format!(
            "this block is a command, but a block that returns {} is wanted here: decode its output (`return !{{… | from-json}}`) or `return` one",
            describe(value)
        ),
        (None, false, _) => format!(
            "a command is wanted here, and this returns {}: write it out: `echo …`, `to-json …`",
            describe(value)
        ),
    })
}

/// The hint for two arms of a form that disagree.  `form` names what the
/// arms are (`branch`, `arm`, `outcome`).
fn arm_join_hint(form: &str) -> String {
    format!(
        "exactly one {form} happens, so there is a single type that every {form} must \
         produce: convert the odd one to that type, or have every one return a tagged \
         value and `case` on it downstream"
    )
}

/// An arm that is no block, where an `if` or `case` forces one.
fn not_a_block(kind: &TypeErrorKind) -> bool {
    matches!(kind, TypeErrorKind::TyMismatch { expected, actual }
        if matches!(**expected, Ty::Thunk(_)) != matches!(**actual, Ty::Thunk(_)))
}

/// What a producer's value is, for a sentence: a command's is its output.
fn describe_producer(cty: &CompTy) -> String {
    match cty {
        CompTy::Return(Grade::Output, _) => "its output, being a command".to_string(),
        CompTy::Return(_, ty) => describe(ty),
        CompTy::Fun(..) | CompTy::Var(_) => "something else".to_string(),
    }
}

/// The hint for an arm that stands in for a head and produces the wrong
/// thing: `expected` is what the head produces, `actual` what the arm does.
fn stands_in_hint(standing: &Standing, kind: &TypeErrorKind) -> String {
    let TypeErrorKind::CompTyMismatch { expected, actual } = kind else {
        return "an arm produces what the head it stands in for produces".to_string();
    };
    let returned = describe_producer(actual);
    match standing {
        Standing::Command(head) => format!(
            "an arm for `{head}` stands in for a command, so its value is its output; \
             this one returns {returned}: write it, `echo …`"
        ),
        Standing::Own(head) => format!(
            "an arm for `{head}` stands in for the `{head}` in force, so it produces what \
             that produces: {}; this one returns {returned}",
            describe_producer(expected)
        ),
        Standing::EveryCommand => format!(
            "the catch-all `handler:` stands in for every command in this block, so its \
             value is its output; this one returns {returned}: write it, `echo …`, or \
             handle specific names with `handlers: [name: …]`"
        ),
    }
}

/// The hint for a non-final pipeline stage that returns instead of writing.
fn stage_writes_hint(stage: Option<&str>, next: Option<&str>, kind: &TypeErrorKind) -> String {
    let Some(stage) = stage else {
        return "a value in stage position writes nothing to the pipe: a stage feeds the \
                next by writing, so write it with `echo`, or put a command here"
            .to_string();
    };
    let returned = value_sides(kind).map_or_else(
        || "a value".to_string(),
        |(expected, actual)| {
            describe(if *actual == Ty::Unit {
                expected
            } else {
                actual
            })
        },
    );
    let reaches = next.map_or_else(|| "the next stage".to_string(), |next| format!("`{next}`"));
    format!(
        "a stage feeds the next by writing, and `{stage}` returns {returned} instead: \
         nothing reaches {reaches}. Did you mean `echo !{{{stage} …}} | {}`, or to bind it, \
         `let x = {stage} …`?",
        next.unwrap_or("…")
    )
}

fn fmt_case_exhaustiveness(missing: &[String], extra: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    match missing {
        [] => {}
        [one] => parts.push(format!("no arm for {}", code(one))),
        many => parts.push(format!("no arms for {}", coded(many))),
    }
    match extra {
        [] => {}
        [one] => parts.push(format!(
            "arm for {} but the value never produces it",
            code(one)
        )),
        many => parts.push(format!(
            "arms for {} but the value never produces them",
            coded(many)
        )),
    }
    format!("case is not exhaustive: {}", parts.join("; "))
}

/// What a mismatch on a weak variable adds: the one thing the types do not
/// show, that another use in the unit already fixed this one, and what makes
/// the type one per unit.
fn weak_note(source: &WeakSource) -> String {
    const SHARED: &str = "this type is shared by every use in its unit, and another use fixed it: ";
    match source {
        WeakSource::Boundary(name) => format!(
            "{SHARED}what `{name}` reads has one type for the whole unit, which its door checks \
             every use of. Write one function per shape, or decode once and bind the result"
        ),
        WeakSource::Host(name) => format!(
            "{SHARED}the host set `${name}` without a type, so it has one type for the whole unit"
        ),
        WeakSource::Residual => format!(
            "{SHARED}data of a shape the program did not decide keeps its type across units. \
             Write one function per shape, or decode once and bind the result"
        ),
    }
}

impl TypeError {
    /// Spanless when `pos` is `None`; the secondary label is dropped when the
    /// witness lies in another file.
    pub fn report(&self) -> Report {
        let at = |span, text| diagnostic::Label {
            span: Some(span),
            text,
        };
        Report {
            code: Some(self.kind.code()),
            message: self.message(),
            at: self.pos.map(|sp| at(sp, self.label())),
            also: self
                .pos
                .zip(self.kind.witness(self.pos))
                .filter(|(sp, (w, _))| w.file == sp.file)
                .map(|(_, (w, text))| at(w, text)),
            hint: self.hint(),
        }
    }
}

/// Guidance, then default, weak-variable and `()` notes when applicable.
pub(super) fn hint(
    kind: &TypeErrorKind,
    reason: Option<&Reason>,
    weak: Option<&WeakSource>,
    unit: Option<&UnitCall>,
) -> Option<String> {
    let lines: Vec<String> = guidance(kind, reason)
        .into_iter()
        .chain(default_note(reason))
        .chain(weak.map(weak_note))
        .chain(unit.filter(|_| concerns_unit(kind)).map(unit_note))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// A default is chosen only because the enclosing definition left the head undecided.
fn default_note(reason: Option<&Reason>) -> Option<String> {
    match reason? {
        Reason::Binding { defaulted_at: Some(_) } => Some(
            "nothing in the enclosing definition makes the right-hand side a command or a function, so the binding runs it for its value".into(),
        ),
        Reason::Call { defaulted_at: Some(_), .. } => Some(
            "nothing in the enclosing definition makes the head a computation returning a block, so the application calls it as a function".into(),
        ),
        _ => None,
    }
}

/// The two uses a conflicting default cites, each with what it needs.
fn conflicting_uses(
    binding: Option<Span>,
    call: Option<Span>,
) -> [(Option<Span>, &'static str); 2] {
    [
        (binding, "binding needs a value-producing computation here"),
        (call, "application needs a function here"),
    ]
}

/// Both readings of a computation that is run and called, named as the
/// program names it.  Elaboration writes `!$x` and a bare `x` alike, so
/// neither spelling is quoted back.
fn conflicting_defaults_hint(forced: Option<&str>, callee: Option<&str>) -> String {
    let returns =
        "if it returns a function, keep the binding and call the function it binds instead";
    match (forced, callee) {
        (Some(name), _) => format!(
            "if `{name}` is a function, bind it with `${name}` rather than running it; {returns}"
        ),
        (None, Some(name)) => {
            format!("if `{name}` is a function, bind it without running it; {returns}")
        }
        (None, None) => {
            format!("if the computation is a function, bind it without running it; {returns}")
        }
    }
}

/// Whether an error is about `()` being used as something.
fn concerns_unit(kind: &TypeErrorKind) -> bool {
    match kind {
        TypeErrorKind::KindMismatch {
            found: KindFound::Type(ty),
            ..
        } => **ty == Ty::Unit,
        TypeErrorKind::TyMismatch { expected, actual } => {
            **expected == Ty::Unit || **actual == Ty::Unit
        }
        TypeErrorKind::ExecArgNotText { refused, .. } => *refused == RefusedArg::Unit,
        _ => false,
    }
}

/// Why a name is `()`: the `let` that bound it took what a call returns.
fn unit_note(call: &UnitCall) -> String {
    let UnitCall { name, callee } = call;
    format!(
        "`${name}` is `()`: `let {name} = {callee} …` binds what `{callee}` returns, and \
         `{callee}` returns `()`. If `{callee}` prints what you want, capture it: \
         `let {name} = {callee} … | from-line`"
    )
}

/// The guidance sentence: keyed on the kind where the kind is its own complete
/// diagnosis, otherwise on the [`Reason`] the failed constraint was raised
/// under.  That second match is wildcard-free on purpose, so a new `Reason`
/// must be given prose or listed as hintless.
fn guidance(kind: &TypeErrorKind, reason: Option<&Reason>) -> Option<String> {
    if let TypeErrorKind::KindMismatch {
        found,
        kind: required,
        ..
    } = kind
    {
        return reason
            .and_then(|reason| read_hint(kind, reason))
            .or_else(|| kind_hint(found, *required, reason));
    }
    if let Some(Reason::Vocabulary(vocab)) = reason {
        match kind {
            // The message lists the tags, so the row hint would say it twice.
            TypeErrorKind::RowExtraField {
                label: Label::Case(tag),
                ..
            } => return vocab.miss(tag),
            TypeErrorKind::TyMismatch { expected, actual } => {
                if let Some(hint) = vocab_mismatch(vocab, expected, actual) {
                    return Some(hint);
                }
            }
            _ => {}
        }
    }
    let from_kind = match kind {
        TypeErrorKind::TypeTooDeep => Some(
            "reduce the type's nesting; composing functions that repeatedly wrap their input can exceed the supported depth".into(),
        ),
        TypeErrorKind::ConflictingDefaults { forced, callee, .. } => Some(
            conflicting_defaults_hint(forced.as_deref(), callee.as_deref()),
        ),
        TypeErrorKind::CommandNotFunction { .. } => Some(
            "a command head must be a function or a thunk; \
             a value here is data, not something you can invoke: pass it \
             as an argument, or wrap it in a function instead"
                .to_string(),
        ),
        TypeErrorKind::CyclicType { .. } => Some(
            "ral allows a recursive type only through a list, map, record or variant \
             (a stream, a tree). Did you forget an argument?"
                .to_string(),
        ),
        TypeErrorKind::RefusedKey { advice, .. } => Some((*advice).to_string()),
        // `handlers:` is the parser's, so `offered` cannot name it.
        TypeErrorKind::UnknownKey { form: "within", .. } => Some(
            "`within` also takes `handlers:`, written as a list of arms: \
             `within [handlers: [deploy: { |args| … }]] { … }`"
                .to_string(),
        ),
        TypeErrorKind::ReturnNotRecord {
            form,
            found,
            offered,
        } => Some(return_not_record_hint(form, found, offered)),
        TypeErrorKind::ControlOperatorAsValue { name } => Some(format!(
            "did you mean to invoke `{name}` as a command (e.g. `{name} ...`)?"
        )),
        TypeErrorKind::HandlerNotFirstClass { .. } => Some(
            "aliases and `within` handlers are command handlers; use command position to invoke them"
                .to_string(),
        ),
        TypeErrorKind::BuiltinNotFirstClass { name } => {
            Some(format!("did you mean to invoke `{name} ...` in command position?"))
        }
        TypeErrorKind::UnboundVariable {
            name,
            suggestions,
            boundary,
        } => Some(match boundary {
            Some(prefix) => hyphen_boundary_hint(prefix),
            None => unbound_variable_hint(name, suggestions),
        }),
        TypeErrorKind::HeadBoundToValue { name, .. } => Some(format!(
            "`${name}` is the value; if you meant a command named `{name}`, `^{name}` runs it"
        )),
        TypeErrorKind::BuiltinArity { name, .. } => {
            Some(format!("`explain {name}` gives its command shape"))
        }
        // Name the rewrite, not merely the refusal: this is the one place the
        // rule costs a user a program that ran.
        TypeErrorKind::SpreadIntoApplication { head } => Some(match head {
            SpreadHead::Builtin { name, arity } if *arity == 1 => {
                format!("pass the element itself, as in `{name} $xs[0]`, or spread into a command")
            }
            SpreadHead::Builtin { name, arity } => format!(
                "pass the {} one at a time, as in `{name} $xs[0] $xs[1]`, \
                 or spread into a command",
                plural(*arity, "argument")
            ),
            SpreadHead::Applied => {
                "index the list at the call, as in `$f $xs[0]`, or spread into a command"
                    .to_string()
            }
        }),
        TypeErrorKind::DecoderTakesNoArgument { .. } => Some(
            "to decode a value in hand, pipe it through the matching encoder: \
             `to-string $x | from-json`"
                .to_string(),
        ),
        TypeErrorKind::DecoderMidPipeline { name, .. } => Some(format!(
            "bind the value first (`let v = … | {name}`) and work on `$v`, or drop the \
             decoder to keep the bytes flowing"
        )),
        // Phrased as the runtime phrases its own missing-key hint, so a reader
        // meeting the two errors meets one language.
        TypeErrorKind::RowExtraField { label, known }
            if !known.is_empty() && !matches!(reason, Some(Reason::RecordUpdate { .. })) => {
            Some(match label {
                Label::Field(_) => format!(
                    "available: {}; did you mean one of those?",
                    known.join(", ")
                ),
                Label::Case(tag) => {
                    let all = coded(known);
                    match near_tags(tag, known)[..] {
                        [near] => format!("available tags: {all}; did you mean {}?", code(near)),
                        _ => format!("available tags: {all}"),
                    }
                }
            })
        }
        TypeErrorKind::NotATable { key, first, .. } => Some(format!(
            "a grant table is a record; for `{key}` to admit nothing write `{key}: [{first}: []]`"
        )),
        TypeErrorKind::DuplicateField { label } => Some(format!(
            "a record has one value per field, so keep whichever '{label}' you meant; \
             to update a field a spread supplies, write it once after the spread, \
             as in `[...$m, {label}: …]`"
        )),
        // The remedy is the shape's own, and the shape's own is where the
        // spawn-time refusal reads it too.
        TypeErrorKind::ExecArgNotText {
            command, refused, ..
        } => Some(refused.remedy(command)),
        TypeErrorKind::MalformedAlias { detail } | TypeErrorKind::MalformedUnalias { detail } => {
            Some(detail.to_string())
        }
        TypeErrorKind::IndexOnLiteral { literal, key } => Some(format!(
            "did you mean to index what a command returns? Force the command first, \
             then index its result: `!{{… {literal}}}[{key}]`"
        )),
        TypeErrorKind::CaseOnNonVariant { .. } => {
            Some(format!(
                "construct the value with a tag ({}) before scrutinising it",
                code("`name payload")
            ))
        }
        // Both remedies keep every command the program already names: one drops
        // the wire, the other keeps the producer and gives it somewhere to run.
        TypeErrorKind::DeadPipeEdge { .. } => Some(
            "drop the pipe, or run the stage's producer as its own statement \
             (`spawn` it, if the two were meant to run at once)?"
                .to_string(),
        ),
        _ => None,
    };
    if from_kind.is_some() {
        return from_kind;
    }
    if let Some(hint) = grade_hint(kind, reason) {
        return Some(hint);
    }
    if matches!(reason, Some(Reason::IfBranches)) && not_a_block(kind) {
        return Some(
            "an `if` arm is a block, as in `if $c { … } else { … }`, or a name holding one"
                .to_string(),
        );
    }
    if reason.is_some_and(meets_as_peers)
        && let Some(shape) = shape_hint(kind)
    {
        return Some(shape);
    }

    match reason? {
        Reason::Binding { .. } => Some(
            "a binding runs a value producer, captures a command's output, or keeps a function as a block; the bound value must agree with that choice".into(),
        ),
        Reason::Call { recursive_call: false, .. } => Some(
            "an application needs a function, or a computation returning a block to call; its argument and result must agree with that function".into(),
        ),
        Reason::Head { .. } => Some(
            "a call's head must be a block, not data: pass the value as an argument instead".into(),
        ),
        Reason::ListPattern => Some(
            "the pattern `[a, b, ...]` only destructures a list: \
             the value being bound has to be a list of the same shape"
                .to_string(),
        ),
        Reason::RecordPattern => Some(
            "the pattern `[key: name, ...]` only destructures a \
             record: the value being bound has to be a record \
             with at least the named fields"
                .to_string(),
        ),
        Reason::Argument => argument_hint(kind).or_else(|| {
            Some(
                "the function's parameter type and the argument's type \
                 must agree: check what the function expects and what \
                 you're passing in"
                    .to_string(),
            )
        }),
        Reason::NotOperand => Some(
            "`not` flips a Bool: its operand has to be a Bool (`true` / `false` or a comparison)"
                .to_string(),
        ),
        Reason::MapKey => Some("map keys must be Strings: convert with `str`".to_string()),
        Reason::ListSpread => list_spread_shape_hint(kind).or_else(|| {
            Some(
                "a `...x` spread copies the elements of a \
                 list into this position, so the value \
                 after `...` must itself be a list"
                    .to_string(),
            )
        }),
        r @ (Reason::MapIndexKey
        | Reason::RecordFieldRead
        | Reason::MapKeyRead
        | Reason::ListPositionRead) => read_hint(kind, r),
        Reason::CaseArmHandler => Some(
            "an arm that names its handler runs that handler on the payload, \
             so the name must stand for a function of one argument: to hand \
             a value back instead, write the arm out, as in \
             `ok: { |_| return 5 }"
                .to_string(),
        ),
        Reason::CaseArms => Some(arm_join_hint("arm")),
        Reason::ForceOperand => Some(
            "the `!` operator runs a block: its operand must be a \
             block value (something built with `{ ... }`), not data"
                .to_string(),
        ),
        Reason::IfCond => Some(
            "the condition of an `if` must be a Bool: either `true`/`false` \
             or an expression that produces one (e.g. `$[$x == 1]`)"
                .to_string(),
        ),
        Reason::IfBranches => Some(arm_join_hint("branch")),
        Reason::TryArms => Some(arm_join_hint("outcome")),
        Reason::StandsIn(standing) => Some(stands_in_hint(standing, kind)),
        Reason::PipelineStageWrites { stage, next } => {
            Some(stage_writes_hint(stage.as_deref(), next.as_deref(), kind))
        }
        Reason::BinaryOperands(op) => Some(
            match op {
                BinaryOp::Arith(ArithOp::Mod) => "`%` takes Ints: a float has no remainder here",
                BinaryOp::Arith(_) => {
                    "the two sides of a `+` / `-` / `*` / `/` must have the same numeric type"
                }
                BinaryOp::Compare(_) => {
                    "you can only compare two values of the same type with `<` / `>` / `<=` / `>=`"
                }
                BinaryOp::Eq(_) => {
                    "you can only check equality between two values of the same type"
                }
            }
            .to_string(),
        ),
        Reason::PipelineStageShape { .. } => Some(
            "a pipeline stage must be ready to run, not still waiting for an \
             argument: apply it to its argument (`f $x`) rather than piping \
             into it, or read the incoming bytes with a decoder such as \
             `from-line` if it should consume the stream instead"
                .to_string(),
        ),
        Reason::DiscardedValueShape { .. } => Some(
            "a discarded statement's value is thrown away, so it must be \
             ready to run, not still waiting for an argument: apply it to \
             what's left, or write `$name` if you meant to keep the function \
             itself"
                .to_string(),
        ),
        Reason::RedirectShape => Some(
            "a redirect needs a computation ready to run, not a function still waiting for \
             an argument: apply the missing arguments before redirecting (`f $x > out`), \
             or put the redirect inside a block (`{ |x| f $x > out }`)"
                .to_string(),
        ),
        Reason::OptionField { form, key } => Some(format!("{form} {key}: wrong value type")),
        Reason::HandlerArm => Some(
            "an arm installed under a name stands in for that command, so it is a block \
             the call runs: `[handlers: [deploy: { |args| … }]]`, or a name bound to one"
                .to_string(),
        ),
        Reason::ListElem => Some(
            "a list holds one type, and this element's differs from its neighbours': \
             to keep different things side by side, write a record, as in \
             `[name: $n, status: $s]`; for alternatives, tag each element and `case` \
             on it downstream"
                .to_string(),
        ),
        Reason::MapElem => Some(
            "a map's keys are data, so every value must have the one type; if the \
             values are genuinely different shapes, write the keys out as labels \
             (`[a: 1, b: \"two\"]`): that is a record, and a record's fields each \
             keep their own type"
                .to_string(),
        ),
        Reason::RecordUpdate { base } => Some(record_update_hint(base.as_deref(), kind)),
        Reason::MapSpread => Some(
            "a `...x` spread inside a map literal (one with a computed key or a `[:,` \
             opener) copies another map's entries into it, so the value after `...` must \
             itself be a map: a record's fields are reached by name, so spread it into \
             a record literal (`[...$r, k: v]`) instead"
                .to_string(),
        ),
        Reason::ScopeBody => Some(
            "`within`, `grant`, `try`, `guard`, `audit` and `spawn` each run a block: the \
             body has to be a block value (something built with `{ ... }`), not data"
                .to_string(),
        ),
        Reason::SpawnTitle => {
            Some("a spawn's title is a String naming the work to people".to_string())
        }
        Reason::TryHandler => Some(
            "a `try` handler is given the error the body raised, so it has to be a \
             block of one argument: `try { … } { |e| … }`"
                .to_string(),
        ),
        Reason::ReturnShape => Some(
            "this position needs a computation that is ready to run, and this one is \
             still waiting for an argument: apply it to what's missing (`f $x`), or \
             write `$name` if you meant to hand over the function itself"
                .to_string(),
        ),
        Reason::LetRecSelf | Reason::Call { recursive_call: true, .. } => Some(
            "a definition that calls itself is checked at one single type throughout \
             its own body, so every call it makes to itself must use the same argument \
             and result types as the rest of the definition"
                .to_string(),
        ),
        Reason::AliasParam => Some(
            "a handler or an alias is invoked as a command, so its one parameter is \
             the argv: a list of strings. Write the arm `{ |args| … }` and read \
             `$args[0]`, or call the function directly, as in `!{$f 1}`"
                .to_string(),
        ),
        Reason::CatchAllParams => Some(
            "the catch-all stands in for every command, so its two parameters are the \
             name and the argv: write `{ |name args| … }`"
                .to_string(),
        ),
        Reason::Factory => Some(
            "a plugin returns its manifest, or a block of one parameter, the options, \
             that makes it: `return { |options| [name: …] }`"
                .to_string(),
        ),
        // These constraints cannot fail on user input, already have a complete
        // diagnosis, or apply only to checker-inserted nodes.
        Reason::CaseScrutinee
        | Reason::CaseCoverage { .. }
        | Reason::Negation
        | Reason::Interpolation
        | Reason::ProtectContract
        | Reason::Vocabulary(_)
        | Reason::Capture => None,
    }
}

/// A spread updates the fields its record has and adds none, so a label the
/// record lacks gets the misspelling or the three idioms; any other failure
/// is a spread of something that is no record.
fn record_update_hint(base: Option<&str>, kind: &TypeErrorKind) -> String {
    let TypeErrorKind::RowExtraField { label, known } = kind else {
        return "a `...x` spread inside a record literal updates another record's fields, \
                so the value after `...` must itself be a record: a map's keys are data, \
                not labels, so a map has no fields to update"
            .to_string();
    };
    let label = label.name();
    let subject = base.map_or_else(|| "this record".to_string(), |b| format!("`${b}`"));
    let lead = format!(
        "{subject} has no field `{label}` to update: a spread replaces fields its record already has"
    );
    if let [near] = crate::text::near_names(label, known.iter().map(String::as_str), 1)[..] {
        return format!("{lead} (did you mean `{near}`?)");
    }
    let r = base.unwrap_or("r");
    format!(
        "{lead}; to carry a new field, give the record the field from the start, \
         nest it (`[{r}: ${r}, total: …]`), or make `${r}` a map (`[:, ...${r}, total: …]`)"
    )
}

/// A spread that mismatches because the value is a record or a map, not a
/// list — blaming `[...x]`'s list shape rather than the caller's value. Which
/// side lands in `expected` is an accident of the call site, so both are checked.
fn list_spread_shape_hint(kind: &TypeErrorKind) -> Option<String> {
    let TypeErrorKind::TyMismatch { expected, actual } = kind else {
        return None;
    };
    let keyed = |t: &Ty| matches!(t, Ty::Record(_) | Ty::Map(_));
    (keyed(expected) || keyed(actual)).then(|| {
        "this is a list literal, and `...` here copies list elements: a record \
         update is written as a record literal (`[...$a, port: 1]`), a map merge as \
         `union $a $b`"
            .to_string()
    })
}

/// An argument of a familiar shape where its neighbour was wanted: text for a
/// list, or a record for one of its `String` fields.  Which side lands in
/// `expected` is an accident of the call site, so both orders are checked.
fn argument_hint(kind: &TypeErrorKind) -> Option<String> {
    let (expected, actual) = value_sides(kind)?;
    [(expected, actual), (actual, expected)]
        .into_iter()
        .find_map(|pair| match pair {
            (Ty::List(_), Ty::String) => Some(
                "a String is not a list: `slice $s 0 n` cuts text and `lines $s` splits \
                 it into a list; a list is cut with `take`/`drop`, and strings join by \
                 interpolation, `\"$a$b\"`, or `intercalate '' $xs`"
                    .to_string(),
            ),
            (Ty::Record(row), Ty::String) => {
                let text = row.fields().find_map(|(label, ty)| match (label, ty) {
                    (Label::Field(name), Ty::String) => Some(name),
                    _ => None,
                })?;
                Some(format!(
                    "this is a record {}: did you mean one of its fields, \
                     as in `$x[{text}]`?",
                    available_fields(row)?
                ))
            }
            _ => None,
        })
}

/// Renders the labels of a record's known fields as `(available: a, b)`; an open row's tail is not listed.
fn available_fields(row: &Row) -> Option<String> {
    let fields: Vec<_> = row.fields().map(|(label, _)| label.name()).collect();
    (!fields.is_empty()).then(|| format!("(available: {})", fields.join(", ")))
}

/// Reasons where two types meet on equal footing, so a hint read off
/// their shapes beats the reason's own sentence.
fn meets_as_peers(reason: &Reason) -> bool {
    matches!(
        reason,
        Reason::Argument | Reason::IfBranches | Reason::CaseArms | Reason::TryArms
    )
}

/// Help for a value mismatch derived from the types' own shapes, not from which
/// reason raised it — a `Ty::Thunk` parameter, an error-record `[status: Int |
/// r]`, and a byte/list confusion each name what the mismatch *is*, whether it
/// arose at an argument, a spread, or a branch join. Which side lands in
/// `expected` is an accident of the call site, so every test here considers
/// both orders.
fn shape_hint(kind: &TypeErrorKind) -> Option<String> {
    let (expected, actual) = value_sides(kind)?;
    let is_record = |t: &Ty| matches!(t, Ty::Record(_));
    let is_map = |t: &Ty| matches!(t, Ty::Map(_));
    if (is_record(expected) && is_map(actual)) || (is_map(expected) && is_record(actual)) {
        return Some(
            "a record and a map are different types over the same pairs: a record's fields \
             are reached by name (`$r[a]`), while a map's keys are data; if these keys are \
             data, build a map: `[$k: v]`, `[:, a: 1, b: 2]` for keys written out, and \
             `[:]` for the empty one"
                .to_string(),
        );
    }
    let is_thunk = |t: &Ty| matches!(t, Ty::Thunk(_));
    if is_thunk(expected) != is_thunk(actual) {
        return Some(
            "one side is a block value (something built with `{ ... }` or `|x| ...`) \
             and the other isn't"
                .to_string(),
        );
    }
    let is_variant = |t: &Ty| matches!(t, Ty::Variant(_));
    if is_variant(expected) != is_variant(actual) {
        return Some(
            "one of these is a tagged value (something built with a backtick, like \
             `` `ok 1 ``) and the other is not: tag the plain one, or take the tagged \
             one apart with `case` first"
                .to_string(),
        );
    }
    let is_error_record = |t: &Ty| matches!(t, Ty::Record(row) if row_has_reason(row));
    if is_error_record(expected) || is_error_record(actual) {
        return Some(
            "`fail` takes a String; use `raise $e` to re-raise an error record, \
             or `describe $e` for its text"
                .to_string(),
        );
    }
    byte_writer_hint(expected, actual)
}

/// A read refusal, by its written key and the head found.
fn read_hint(kind: &TypeErrorKind, reason: &Reason) -> Option<String> {
    let accepts: fn(&Ty) -> bool = match reason {
        Reason::RecordFieldRead => |ty| matches!(ty, Ty::Record(_)),
        Reason::ListPositionRead => |ty| matches!(ty, Ty::List(_)),
        Reason::MapKeyRead => |ty| matches!(ty, Ty::Map(_)),
        Reason::MapIndexKey => |ty| matches!(ty, Ty::String),
        _ => return None,
    };
    let found = match kind {
        TypeErrorKind::TyMismatch { expected, actual } => [&**expected, &**actual]
            .into_iter()
            .find(|ty| !accepts(ty))?,
        TypeErrorKind::KindMismatch {
            found: KindFound::Type(ty),
            ..
        } => ty,
        _ => return None,
    };
    Some(
        match (reason, found) {
            (Reason::RecordFieldRead, Ty::Map(_)) => {
                "a key written out, like `HOME`, names a record field, and this is a map, whose \
             keys are data: read it with a computed key (`$m[$k]`) or with `get $m HOME default`"
            }
            (Reason::RecordFieldRead, Ty::List(_)) => {
                "a list is read by position: `$xs[0]`, or `nth $i $xs` for a computed one"
            }
            (Reason::MapKeyRead, Ty::Record(_)) => {
                "a computed key reads a map, and this is a record, whose fields have names: \
             write the name, as in `$r[host]`"
            }
            (Reason::MapKeyRead, Ty::List(_)) => {
                "a computed key reads a map; for a list element at a computed position, \
             write `nth $i $xs`"
            }
            (Reason::ListPositionRead, Ty::Record(_) | Ty::Map(_)) => {
                "a whole number reads a list element; a field whose name is digits is written \
             quoted, as in `$r['0']`"
            }
            (Reason::MapIndexKey, Ty::Int) => {
                "a computed key reads a map, whose keys are Strings; for a list position, \
             write `nth $i $xs`"
            }
            (Reason::MapIndexKey, ty) if !matches!(ty, Ty::String) => {
                "a computed key reads a map, whose keys are Strings; convert it with `str`"
            }
            (Reason::RecordFieldRead | Reason::ListPositionRead | Reason::MapKeyRead, ty) => {
                match ty {
                    Ty::Thunk(_) => {
                        "run the block first, then index its result: `!{!$t}[field]` \
             (`!$t[field]` reads `field` off `$t` and forces *that*)"
                    }
                    Ty::Bytes => "decode bytes first: `bytes-to-string $b`",
                    Ty::Handle(..) => "await the handle first: `await $h`",
                    Ty::String => "text is not read with `[…]`; use `slice $s 0 80` or `lines $s`",
                    Ty::Unit | Ty::Bool | Ty::Int | Ty::Float => {
                        "only records, lists and maps are read with `[…]`"
                    }
                    _ => return None,
                }
            }
            _ => return None,
        }
        .to_string(),
    )
}

/// Whether a row carries `reason`. An error carries the row as it stood
/// when the constraint failed, and nothing re-applies it here, so a `Var` tail
/// reads as absent.  That only ever withholds a hint, never asserts one.
fn row_has_reason(row: &Row) -> bool {
    row.fields()
        .any(|(label, _)| *label == Label::Field("reason".into()))
}

/// A list where `Bytes` was wanted, or the reverse.  Every byte-channel writer
/// takes one argument type, so the hint must hold for each of them.
fn byte_writer_hint(expected: &Ty, actual: &Ty) -> Option<String> {
    let list_for_bytes = |a: &Ty, b: &Ty| matches!(a, Ty::List(_)) && matches!(b, Ty::Bytes);
    (list_for_bytes(expected, actual) || list_for_bytes(actual, expected)).then(|| {
        "Bytes are not a list: read them as text with `bytes-to-string $b`, or as \
         lines with `lines !{bytes-to-string $b}`; a list of numbers is written as \
         Bytes by `ints-to-bytes`, a list of lines by `to-lines`"
            .to_string()
    })
}

/// Why `$STATUS` names nothing; the runtime's undefined-variable error says
/// the same.
pub(crate) const NO_STATUS_REGISTER: &str = "there is no status register: a failure raises an \
    error record: catch it with `try` and read `$e[reason]` from the handler's argument";

/// The message and hint refusing `...` in a value's application, which the
/// elaborator raises for a head it knows to be a value: the checker's words.
pub(crate) fn spread_into_application() -> (String, String) {
    let kind = TypeErrorKind::SpreadIntoApplication {
        head: SpreadHead::Applied,
    };
    let hint = guidance(&kind, None).expect("a refused spread names its rewrite");
    (kind.render_message(), hint)
}

/// The longest prefix of `name` that ends just before a `-` and is bound:
/// `-` continues a name, so `$os-$arch` reads `$os-`.
pub(crate) fn hyphen_boundary(name: &str, is_bound: impl Fn(&str) -> bool) -> Option<&str> {
    name.rmatch_indices('-')
        .map(|(i, _)| &name[..i])
        .find(|p| !p.is_empty() && is_bound(p))
}

pub(crate) fn hyphen_boundary_hint(prefix: &str) -> String {
    format!("`${prefix}` is defined, and `-` continues a name: write `$({prefix})` to end it there")
}

/// What an unbound `$name` most likely meant, most specific first.
fn unbound_variable_hint(name: &str, suggestions: &[String]) -> String {
    if name == "STATUS" {
        return NO_STATUS_REGISTER.into();
    }
    let names: Vec<_> = suggestions.iter().map(|s| format!("`${s}`")).collect();
    if let Some((last, init)) = names.split_last() {
        let lead = if init.is_empty() {
            String::new()
        } else {
            format!("{} or ", init.join(", "))
        };
        return format!("did you mean {lead}{last}?");
    }
    if name
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return format!("environment variables are read as `!{{env}}[{name}]`");
    }
    "check the spelling, or define it with `let` before this line".into()
}

/// What a contract file that returns no record should return instead.
fn return_not_record_hint(form: &str, found: &Ty, offered: &[&str]) -> String {
    let key = offered.first().copied().unwrap_or("key");
    match found {
        Ty::Map(_) => format!(
            "the keys of `{form}` are its labels, so write a record: `[{key}: …]`, \
             not a map (`[:, {key}: …]`)"
        ),
        Ty::List(_) => format!(
            "`[]` is the empty list, not the empty record; a file with nothing to set \
             returns `()`, and otherwise `{form}` wants a record such as `[{key}: …]`"
        ),
        Ty::Thunk(_) => format!("`{form}` returns its settings, not a block"),
        _ => format!("return a record such as `[{key}: …]`, or `()` for nothing to set"),
    }
}

/// The tags of `known` within edit distance of `tag`, as written, nearest first.
fn near_tags<'a>(tag: &str, known: &'a [String]) -> Vec<&'a str> {
    let bare = |k: &'a String| k.trim_start_matches(TAG_PREFIX);
    let near = crate::text::near_names(tag, known.iter().map(bare), 1);
    known
        .iter()
        .filter(|k| near.contains(&bare(k)))
        .map(String::as_str)
        .collect()
}

/// The tags a type offers, peeling the lists around it.
fn offered_tags(ty: &Ty) -> Option<Vec<String>> {
    match ty {
        Ty::List(elem) => offered_tags(elem),
        Ty::Variant(row) => Some(row.fields().map(|(label, _)| label.to_string()).collect()),
        _ => None,
    }
}

/// What a vocabulary says of a value that is no tag of its row, or whose
/// payload is the wrong type.
fn vocab_mismatch(vocab: &Vocab, expected: &Ty, actual: &Ty) -> Option<String> {
    let advice = match (offered_tags(expected), offered_tags(actual)) {
        (Some(_), None) | (None, Some(_)) => vocab.untagged,
        (None, None) => vocab.payload,
        _ => return None,
    };
    Some(advice.to_string())
}

/// Whether one side is a vocabulary's tag and the other is not a tag at all.
fn untagged_mismatch(expected: &Ty, actual: &Ty) -> bool {
    offered_tags(expected).is_some() != offered_tags(actual).is_some()
}

/// Each tag of `known` as a code span, joined as a list.
fn coded(known: &[String]) -> String {
    known.iter().map(|k| code(k)).collect::<Vec<_>>().join(", ")
}

/// The headline for a tag a vocabulary's closed row lacks, in its words: the
/// one sentence the checker and the grant decoder both say.  `known` are the
/// row's tags as written, `` `cwd ``; `suggest` offers the nearest of them.
pub(crate) fn unknown_tag_message(
    noun: &str,
    tag: &str,
    known: &[String],
    suggest: bool,
) -> String {
    let noun = indefinite(noun);
    let near = match near_tags(tag, known)[..] {
        [near] if suggest => format!(" (did you mean {}?)", code(near)),
        _ => String::new(),
    };
    format!(
        "{} is not {noun}; {noun} is one of {}{near}",
        code(&format!("{TAG_PREFIX}{tag}")),
        coded(known)
    )
}

impl TypeError {
    fn is_redirect_shape(&self) -> bool {
        matches!(
            (&self.kind, &self.reason),
            (
                TypeErrorKind::CompTyMismatch { .. },
                Some(Reason::RedirectShape)
            )
        )
    }

    /// The headline: the kind's own, or its vocabulary's when the failed
    /// constraint was against one, or the redirect's when a function met one.
    pub fn message(&self) -> String {
        if self.is_redirect_shape() {
            return "a redirect was applied to a function that still waits for arguments".into();
        }
        match (&self.kind, &self.reason) {
            (
                TypeErrorKind::RowExtraField {
                    label: Label::Case(tag),
                    ..
                },
                Some(Reason::Vocabulary(vocab)),
            ) => Some(vocab.refuse(tag).0),
            (TypeErrorKind::TyMismatch { expected, actual }, Some(Reason::Vocabulary(vocab)))
                if untagged_mismatch(expected, actual) =>
            {
                let wanted = match **expected {
                    Ty::List(_) => format!("a list of {}s", vocab.noun),
                    _ => indefinite(vocab.noun),
                };
                Some(format!("expected {wanted}, got {}", describe(actual)))
            }
            _ => None,
        }
        .unwrap_or_else(|| self.kind.render_message())
    }

    /// The caret label, specialised for a redirect or a conflicting use.
    fn label(&self) -> String {
        if let TypeErrorKind::ConflictingDefaults { binding, call, .. } = &self.kind
            && let Some((_, text)) = conflicting_uses(*binding, *call)
                .into_iter()
                .find(|&(span, _)| span.is_some() && span == self.pos)
        {
            return text.into();
        }
        if self.is_redirect_shape() {
            return "this still needs arguments".into();
        }
        self.kind.render_label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::{CompTy, Typed as _};

    fn mismatch(expected: Ty, actual: Ty) -> TypeErrorKind {
        TypeErrorKind::TyMismatch {
            expected: Box::new(expected),
            actual: Box::new(actual),
        }
    }

    fn producer_mismatch(expected: Ty, actual: Ty, grades: (Grade, Grade)) -> TypeErrorKind {
        TypeErrorKind::CompTyMismatch {
            expected: CompTy::Return(grades.0, Box::new(expected)),
            actual: CompTy::Return(grades.1, Box::new(actual)),
        }
    }

    fn conflicting_defaults(binding: Span, call: Span, pos: Option<Span>) -> TypeError {
        TypeError {
            pos,
            kind: TypeErrorKind::ConflictingDefaults {
                binding: Some(binding),
                call: Some(call),
                forced: Some("f".into()),
                callee: Some("f".into()),
            },
            reason: None,
            weak: None,
            unit: None,
        }
    }

    #[test]
    fn conflicting_defaults_reports_both_uses_and_both_spellings() {
        let binding = Span::new(crate::source::FileId(0), 10, 20);
        let call = Span::new(crate::source::FileId(0), 30, 40);
        for (primary, secondary, primary_text, secondary_text) in [
            (binding, call, "binding", "application"),
            (call, binding, "application", "binding"),
        ] {
            let report = conflicting_defaults(binding, call, Some(primary)).report();
            assert_eq!(report.code, Some("T0080"));
            assert!(report.message.contains("binding"));
            assert!(report.message.contains("application"));
            let at = report.at.unwrap();
            assert_eq!(at.span, Some(primary));
            assert!(at.text.contains(primary_text));
            let also = report.also.unwrap();
            assert_eq!(also.span, Some(secondary));
            assert!(also.text.contains(secondary_text));
            let hint = report.hint.unwrap();
            assert!(
                hint.contains("bind it with `$f` rather than running it;"),
                "{hint}"
            );
            assert!(hint.contains("call the function it binds"), "{hint}");
        }
    }

    #[test]
    fn conflicting_defaults_without_same_file_locations_still_explains_both_readings() {
        let binding = Span::new(crate::source::FileId(0), 10, 20);
        let call = Span::new(crate::source::FileId(1), 30, 40);
        for pos in [Some(binding), Some(call), None] {
            let report = conflicting_defaults(binding, call, pos).report();
            assert!(report.also.is_none());
            assert!(report.hint.is_some());
        }
        let error = TypeError {
            pos: None,
            kind: TypeErrorKind::ConflictingDefaults {
                binding: None,
                call: None,
                forced: None,
                callee: None,
            },
            reason: None,
            weak: None,
            unit: None,
        };
        assert!(error.report().also.is_none());
        let hint = error.hint().unwrap();
        assert!(hint.contains("if the computation is a function"), "{hint}");
        assert!(!hint.contains('$'), "{hint}");
    }

    #[test]
    fn type_too_deep_guidance_takes_priority_over_shape_reasons() {
        for reason in [
            Reason::Head {
                name: Some("d10".into()),
            },
            Reason::Argument,
            Reason::LetRecSelf,
            Reason::Call {
                defaulted_at: None,
                recursive_call: true,
            },
        ] {
            let hint = guidance(&TypeErrorKind::TypeTooDeep, Some(&reason)).unwrap();
            assert!(hint.contains("reduce the type's nesting"), "{hint}");
            assert!(!hint.contains("head must be a block"), "{hint}");
        }
    }

    #[test]
    fn default_reasons_explain_the_head_their_definition_left_undecided() {
        let closing = Span::new(crate::source::FileId(0), 0, 50);
        for (reason, fragment) in [
            (
                Reason::Binding {
                    defaulted_at: Some(closing),
                },
                "right-hand side a command or a function",
            ),
            (
                Reason::Call {
                    defaulted_at: Some(closing),
                    recursive_call: false,
                },
                "head a computation returning a block",
            ),
        ] {
            let hint = hint(&mismatch(Ty::Int, Ty::String), Some(&reason), None, None).unwrap();
            assert!(hint.contains("nothing in the enclosing definition makes"));
            assert!(hint.contains(fragment));
        }
        for reason in [
            Reason::Binding { defaulted_at: None },
            Reason::Call {
                defaulted_at: None,
                recursive_call: false,
            },
        ] {
            let hint = hint(&mismatch(Ty::Int, Ty::String), Some(&reason), None, None).unwrap();
            assert!(!hint.contains("nothing in the enclosing definition makes"));
        }
    }

    #[test]
    fn same_grade_producer_mismatches_keep_value_shape_hints() {
        let shapes = [
            (
                Ty::Record(Row::Empty),
                Ty::map(Ty::String),
                "a record and a map",
            ),
            (Ty::Variant(Row::Empty), Ty::Int, "a tagged value"),
            (
                Ty::Thunk(Box::new(CompTy::pure(Ty::Unit))),
                Ty::Int,
                "a block value",
            ),
            (
                crate::fact::ErrorRecord::ty(),
                Ty::String,
                "use `raise $e` to re-raise an error record",
            ),
            (Ty::Bytes, Ty::list(Ty::Int), "Bytes are not a list"),
        ];
        for (left, right, fragment) in shapes {
            for (expected, actual) in [(&left, &right), (&right, &left)] {
                let plain = mismatch(expected.clone(), actual.clone());
                let wanted = guidance(&plain, Some(&Reason::Argument));
                assert!(wanted.as_deref().is_some_and(|h| h.contains(fragment)));
                for grade in [Grade::Value, Grade::Var(crate::ty::GradeVar(0))] {
                    let producer =
                        producer_mismatch(expected.clone(), actual.clone(), (grade, grade));
                    for reason in [
                        Reason::Argument,
                        Reason::IfBranches,
                        Reason::CaseArms,
                        Reason::TryArms,
                    ] {
                        assert_eq!(
                            guidance(&producer, Some(&reason)),
                            wanted,
                            "{producer:?}: {reason:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn different_grade_producers_do_not_use_payload_hints() {
        let free = Grade::Var(crate::ty::GradeVar(0));
        let other = Grade::Var(crate::ty::GradeVar(1));
        for grades in [(Grade::Value, free), (free, Grade::Value), (free, other)] {
            for (expected, actual) in [
                (Ty::Record(Row::Empty), Ty::map(Ty::String)),
                (Ty::String, Ty::list(Ty::Int)),
            ] {
                let producer = producer_mismatch(expected, actual, grades);
                for (reason, form) in [
                    (Reason::IfBranches, "branch"),
                    (Reason::CaseArms, "arm"),
                    (Reason::TryArms, "outcome"),
                ] {
                    assert_eq!(
                        guidance(&producer, Some(&reason)),
                        Some(arm_join_hint(form))
                    );
                }
            }
        }
    }

    #[test]
    fn command_value_grade_hints_take_priority_over_payload_shapes() {
        for value in [
            crate::fact::ErrorRecord::ty(),
            Ty::Variant(Row::Empty),
            Ty::Thunk(Box::new(CompTy::pure(Ty::Unit))),
            Ty::Bytes,
        ] {
            for (expected, actual, grades) in [
                (value.clone(), Ty::Unit, (Grade::Value, Grade::Output)),
                (Ty::Unit, value, (Grade::Output, Grade::Value)),
            ] {
                let producer = producer_mismatch(expected, actual, grades);
                for reason in [
                    Reason::Argument,
                    Reason::IfBranches,
                    Reason::CaseArms,
                    Reason::TryArms,
                ] {
                    let wanted = grade_hint(&producer, Some(&reason));
                    assert!(wanted.is_some());
                    assert_eq!(guidance(&producer, Some(&reason)), wanted);
                }
            }
        }
    }

    #[test]
    fn same_grade_producer_arguments_keep_value_argument_hints() {
        for (left, right, fragment) in [
            (Ty::list(Ty::Int), Ty::String, "`lines $s`"),
            (
                Ty::Record(Row::Empty).extend("name", Ty::String),
                Ty::String,
                "`$x[name]`",
            ),
        ] {
            for (expected, actual) in [(&left, &right), (&right, &left)] {
                let wanted = guidance(
                    &mismatch(expected.clone(), actual.clone()),
                    Some(&Reason::Argument),
                );
                assert!(wanted.as_deref().is_some_and(|h| h.contains(fragment)));
                for grade in [Grade::Value, Grade::Var(crate::ty::GradeVar(0))] {
                    let producer =
                        producer_mismatch(expected.clone(), actual.clone(), (grade, grade));
                    assert_eq!(guidance(&producer, Some(&Reason::Argument)), wanted);
                }
            }
        }
    }

    #[test]
    fn block_value_hint_is_the_same_in_both_orders() {
        let block = || Ty::Thunk(Box::new(CompTy::pure(Ty::Unit)));
        let forward = shape_hint(&mismatch(block(), Ty::Int));
        let backward = shape_hint(&mismatch(Ty::Int, block()));
        assert!(
            forward
                .as_deref()
                .is_some_and(|h| h.contains("block value"))
        );
        assert_eq!(forward, backward);
    }

    #[test]
    fn text_for_a_list_hint_is_the_same_in_both_orders() {
        let list = || Ty::list(Ty::Var(crate::ty::TyVar(0)));
        let forward = argument_hint(&mismatch(list(), Ty::String));
        let backward = argument_hint(&mismatch(Ty::String, list()));
        assert!(forward.as_deref().is_some_and(|h| h.contains("`lines $s`")));
        assert_eq!(forward, backward);
    }

    #[test]
    fn record_for_text_hint_is_the_same_in_both_orders() {
        let entry = || {
            Ty::Record(Row::Empty)
                .extend("size", Ty::Int)
                .extend("name", Ty::String)
        };
        let forward = argument_hint(&mismatch(entry(), Ty::String));
        let backward = argument_hint(&mismatch(Ty::String, entry()));
        assert!(
            forward
                .as_deref()
                .is_some_and(|h| h.contains("(available: name, size)") && h.contains("`$x[name]`"))
        );
        assert_eq!(forward, backward);
    }

    #[test]
    fn record_without_text_has_no_argument_hint() {
        let point = Ty::Record(Row::Empty).extend("x", Ty::Int);
        assert_eq!(argument_hint(&mismatch(point, Ty::String)), None);
    }
}
