//! User-facing prose for type errors.  `infer.rs` and `unify.rs` raise them
//! as data; every sentence a user reads is a pure function of that data,
//! written here and nowhere else.

use super::contract::unknown_key_message;
use super::error::{CycleVia, KindFound, Reason, SpreadHead, Standing, TypeErrorKind, UnitCall};
use super::fmt::{FmtCtx, fmt_ty_ctx};
use super::kind::Kind;
use super::ty::{CompTy, Label, Row, Ty};
use super::unify::WeakSource;
use crate::serial::plural;
use crate::source::Span;
use crate::syntax::ast::{ArithOp, BinaryOpKind};
use crate::types::RefusedArg;

impl TypeErrorKind {
    /// The headline sentence for this error.
    ///
    /// Symmetric by design: which side of a constraint lands in `expected` is
    /// an accident of the call site, so no message may claim one side is right.
    pub fn render_message(&self) -> String {
        match self {
            Self::RecursiveRow => {
                "infinite row — a record's field list would refer back to itself".into()
            }
            Self::TypeTooDeep => "type nesting exceeds the supported depth".into(),
            Self::CyclicType { via } => format!(
                "this makes a type that contains itself without going through any data — \
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
            Self::RowExtraField { label, .. } => {
                format!("this record has no field named '{label}'")
            }
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
            Self::CommandNotFunction { ty, .. } => {
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
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!(
                    "`{name}` is bound to a value of type {}, so it is not a program",
                    fmt_ty_ctx(ty, &ctx)
                )
            }
            Self::IndexContainerUnknown { names, .. } => {
                let (target, read, key) = match names {
                    Some((t, k)) => (
                        format!("`${t}`"),
                        format!("`${t}[${k}]`"),
                        format!("`${k}`"),
                    ),
                    None => (
                        "this".to_string(),
                        "an index with a computed key".to_string(),
                        "the key".to_string(),
                    ),
                };
                format!(
                    "is {target} a list or a map? {read} indexes a list when {key} is an Int \
                     and a map when it is a String, but nothing in the program fixes which"
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
                format!("`{name}` takes no argument — it reads the byte channel")
            }
            Self::SpreadIntoApplication { head } => {
                let takes = match head {
                    SpreadHead::Builtin { name, arity } => {
                        format!("`{name}` takes {}", plural(*arity, "argument"))
                    }
                    SpreadHead::Applied => "this head takes its arguments".into(),
                };
                format!(
                    "{takes} by application, and `...` spreads an argv — \
                     which only a command, an external, or a handler has"
                )
            }
            // The wording `runtime::command::vet` uses at the spawn, the
            // refusal being the same refusal one step earlier.
            Self::ExecArgNotText {
                command,
                ty: Ty::Unit,
            } => {
                format!("`()` is nothing, so `{command}` has no word to take from it")
            }
            Self::ExecArgNotText { command, ty } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!(
                    "cannot pass {} to external command '{command}'",
                    fmt_ty_ctx(ty, &ctx)
                )
            }
            Self::FailStatusZero => "fail requires a nonzero status".into(),
            Self::MalformedAlias { .. } => "malformed alias definition".into(),
            Self::MalformedUnalias { .. } => "malformed unalias".into(),
            Self::IndexIntoThunk => {
                "this is a block — you can't read a field from it directly".into()
            }
            Self::FieldOnNonRecord { label, ty } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!(
                    "you tried to read `{label}` (a field or a key) from a value of type {}, but only records have fields and only maps have keys",
                    fmt_ty_ctx(ty, &ctx)
                )
            }
            Self::DynamicIndexOnScalar { ty } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!(
                    "can't index a value of type {} with a runtime key",
                    fmt_ty_ctx(ty, &ctx)
                )
            }
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
            Self::CommandNotFunction { ty, .. } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!("{} cannot be invoked as a command", fmt_ty_ctx(ty, &ctx))
            }
            Self::RowExtraField { label, .. } => format!("no field '{label}' in this record"),
            Self::RowMissingField { label } => format!("this record needs field '{label}'"),
            Self::DuplicateField { label } => format!("'{label}' was already given above"),
            Self::RefusedKey { key, .. } => format!("'{key}' is refused here"),
            Self::UnknownKey { key, .. } => format!("no such key: '{key}'"),
            Self::ReturnNotRecord { found, .. } => format!("returns {}", describe(found)),
            Self::CaseNotExhaustive { missing, extra } => {
                match (missing.as_slice(), extra.as_slice()) {
                    ([only], []) => format!("no arm for {only}"),
                    (some, []) => format!("no arm for {}", some.join(", ")),
                    ([], [only]) => format!("arm for {only} that the value never produces"),
                    ([], some) => {
                        format!("arms for {} that the value never produces", some.join(", "))
                    }
                    _ => "case alternatives don't match the value".into(),
                }
            }
            Self::SpreadIntoApplication { .. } => "this spread has no argv to fill".into(),
            Self::ExecArgNotText { .. } => {
                "an external's arguments are words, and this is not one".into()
            }
            Self::DeadPipeEdge { .. } => "this stage reads here, not from the pipe".into(),
            Self::CaseOnNonVariant { .. }
            | Self::ControlOperatorAsValue { .. }
            | Self::HandlerNotFirstClass { .. }
            | Self::BuiltinNotFirstClass { .. }
            | Self::BuiltinArity { .. }
            | Self::DecoderTakesNoArgument { .. }
            | Self::FailStatusZero
            | Self::MalformedAlias { .. }
            | Self::MalformedUnalias { .. }
            | Self::IndexIntoThunk
            | Self::FieldOnNonRecord { .. }
            | Self::DynamicIndexOnScalar { .. } => "here".into(),
            Self::UnboundVariable { .. } => "not defined".into(),
            Self::IndexContainerUnknown { .. } => "a list or a map?".into(),
            Self::HeadBoundToValue { ty, .. } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!("{} is not a command", fmt_ty_ctx(ty, &ctx))
            }
        }
    }
}

impl TypeErrorKind {
    /// The other span an error rests on: where a kind was imposed, or the
    /// binding that holds an index — unless that is where the error itself
    /// stands.
    pub(crate) fn witness(&self, at: Option<Span>) -> Option<(Span, String)> {
        let (found, kind, witness) = match self {
            Self::KindMismatch {
                found,
                kind,
                witness,
            } => (found, kind, witness),
            Self::IndexContainerUnknown {
                holder: Some(holder),
                ..
            } => return Some((*holder, "this binding holds the index".to_string())),
            _ => return None,
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
        Ty::Handle(_) => "a handle",
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
        Kind::INDEXED => "a list or a map".into(),
        Kind::KEY => "an Int key for a list or a String key for a map".into(),
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
        Kind::LABELLED => "a record or map read by a bare label".into(),
        Kind::INDEXED => "a list or map read by a computed key".into(),
        Kind::KEY => "a key: an Int for a list, a String for a map".into(),
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
    if required == Kind::KEY {
        return Some(format!(
            "a list takes an Int key and a map a String key; this key is {}",
            describe(ty)
        ));
    }
    let lead = match reason {
        Some(Reason::BinaryOperands(BinaryOpKind::Arith(ArithOp::Mod))) => Some("`%` takes Ints"),
        Some(Reason::BinaryOperands(BinaryOpKind::Arith(_))) => {
            Some("`+ - * /` work on numbers (Int or Float)")
        }
        Some(Reason::Negation) => Some("`-` negates a number (Int or Float)"),
        Some(Reason::BinaryOperands(BinaryOpKind::Compare(_))) => {
            Some("`<` compares numbers or text")
        }
        Some(Reason::BinaryOperands(BinaryOpKind::Eq(_))) => Some("`==` compares data"),
        Some(Reason::Interpolation) => {
            Some("only text, numbers and `true`/`false` go into a string")
        }
        _ => None,
    };
    let way = match (reason, &**ty) {
        (Some(Reason::BinaryOperands(BinaryOpKind::Arith(op))), Ty::String)
            if *op != ArithOp::Mod =>
        {
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
            Some("a block has no equality and no encoding — use what it computes, `!$f`")
        }
        (_, Ty::Handle(_)) if required == Kind::DATA => {
            Some("a handle has no equality and no encoding — use what `await` gives")
        }
        _ => None,
    };
    match (lead, way) {
        (Some(lead), Some(way)) => Some(format!("{lead}; {way}")),
        (one, other) => one.or(other).map(str::to_string),
    }
}

/// Prose for a `CompTyMismatch`.  Two commands disagree in what they return;
/// `unify.rs` blames no component when the two heads differ in shape —
/// `Return` against `Fun`.
fn fmt_comp_mismatch(expected: &CompTy, actual: &CompTy) -> String {
    let (CompTy::Return(_, expected), CompTy::Return(_, actual)) = (expected, actual) else {
        return "two computations have incompatible shapes — one is a function, the other is not"
            .into();
    };
    let ctx = FmtCtx::for_value_types(&[expected, actual]);
    format!(
        "couldn't match type {} with type {}",
        fmt_ty_ctx(expected, &ctx),
        fmt_ty_ctx(actual, &ctx)
    )
}

/// What a `CompTyMismatch` between two `F` types returns on each side, as
/// `(expected, actual)`.
fn returns_of(kind: &TypeErrorKind) -> Option<(&Ty, &Ty)> {
    match kind {
        TypeErrorKind::CompTyMismatch {
            expected: CompTy::Return(_, expected),
            actual: CompTy::Return(_, actual),
        } => Some((expected, actual)),
        _ => None,
    }
}

/// The non-`()` side of a mismatch between `()` and something else.
fn against_unit(kind: &TypeErrorKind) -> Option<&Ty> {
    let (expected, actual) = match kind {
        TypeErrorKind::TyMismatch { expected, actual } => (&**expected, &**actual),
        other => returns_of(other)?,
    };
    match (expected, actual) {
        (Ty::Unit, other) | (other, Ty::Unit) if *other != Ty::Unit => Some(other),
        _ => None,
    }
}

/// The hint for two arms of a form that disagree: a command that writes
/// joined against a value, or the plain statement that one type is wanted.
/// `form` names what the arms are (`branch`, `arm`, `outcome`).
fn arm_join_hint(kind: &TypeErrorKind, writer: Option<&str>, form: &str) -> String {
    match (against_unit(kind), writer) {
        (Some(other), Some(cmd)) => format!(
            "one {form} runs `{cmd}`, whose output goes to the terminal, so its value is `()`; \
             the other gives {}. To make the command's output the value, capture it: \
             `{cmd} … | from-line`. To print in both, `echo …`",
            describe(other)
        ),
        _ => format!(
            "exactly one {form} happens, so there is a single type that every {form} must \
             produce — convert the odd one to that type, or have every one return a tagged \
             value and `case` on it downstream"
        ),
    }
}

/// An arm that is no block, where an `if` or `case` forces one.
fn not_a_block(kind: &TypeErrorKind) -> bool {
    matches!(kind, TypeErrorKind::TyMismatch { expected, actual }
        if matches!(**expected, Ty::Thunk(_)) != matches!(**actual, Ty::Thunk(_)))
}

/// The hint for an arm that stands in for a command, and returns the wrong
/// thing: `expected` is what the head returns, `actual` what the arm does.
fn stands_in_hint(standing: &Standing, kind: &TypeErrorKind) -> String {
    let returned = returns_of(kind).map_or_else(
        || "something else".to_string(),
        |(_, actual)| describe(actual),
    );
    match standing {
        Standing::Command(head) => format!(
            "an arm for `{head}` stands in for a command, so it writes and returns `()`; \
             this one returns {returned} — write it, `echo …`"
        ),
        Standing::Own(head) => {
            let wanted = returns_of(kind).map_or_else(
                || "what it returns".to_string(),
                |(expected, _)| describe(expected),
            );
            format!(
                "an arm for `{head}` stands in for the `{head}` in force, so it returns what \
                 that returns — {wanted}; this one returns {returned}"
            )
        }
        Standing::EveryCommand => format!(
            "the catch-all `handler:` stands in for every command in this block, so it \
             writes and returns `()`; this one returns {returned} — write it, `echo …`, or \
             handle specific names with `handlers: [name: …]`"
        ),
    }
}

/// The hint for a non-final pipeline stage that returns instead of writing.
fn stage_writes_hint(stage: Option<&str>, next: Option<&str>, kind: &TypeErrorKind) -> String {
    let Some(stage) = stage else {
        return "a value in stage position writes nothing to the pipe — a stage feeds the \
                next by writing, so write it with `echo`, or put a command here"
            .to_string();
    };
    let returned = returns_of(kind).map_or_else(
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
        "a stage feeds the next by writing, and `{stage}` returns {returned} instead — \
         nothing reaches {reaches}. Did you mean `echo !{{{stage} …}} | {}`, or to bind it, \
         `let x = {stage} …`?",
        next.unwrap_or("…")
    )
}

fn fmt_case_exhaustiveness(missing: &[String], extra: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    match missing {
        [] => {}
        [one] => parts.push(format!("no arm for {one}")),
        many => parts.push(format!("no arms for {}", many.join(", "))),
    }
    match extra {
        [] => {}
        [one] => parts.push(format!("arm for {one} but the value never produces it")),
        many => parts.push(format!(
            "arms for {} but the value never produces them",
            many.join(", ")
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
        WeakSource::Index => format!(
            "{SHARED}a computed index `$c[$k]` has one container, key and element type for the \
             whole unit. Write one function per container type"
        ),
        WeakSource::Boundary(name) => format!(
            "{SHARED}what `{name}` reads has one type for the whole unit, which its door checks \
             every use of. Write one function per shape, or decode once and bind the result"
        ),
        WeakSource::Residual => format!(
            "{SHARED}a computed index `$c[$k]` has one container, key and element type for the \
             whole unit, and so does data of a shape the program did not decide. Write one \
             function per container type, or decode once and bind the result"
        ),
    }
}

/// The guidance sentence, then the weak-variable note when `weak`, then the
/// `()` note when `unit` and the error is about `()`.
pub(super) fn hint(
    kind: &TypeErrorKind,
    reason: Option<&Reason>,
    weak: Option<&WeakSource>,
    unit: Option<&UnitCall>,
) -> Option<String> {
    let lines: Vec<String> = guidance(kind, reason)
        .into_iter()
        .chain(weak.map(weak_note))
        .chain(unit.filter(|_| concerns_unit(kind)).map(unit_note))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
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
        TypeErrorKind::ExecArgNotText { ty, .. } => *ty == Ty::Unit,
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
        return kind_hint(found, *required, reason);
    }
    let from_kind = match kind {
        TypeErrorKind::CommandNotFunction {
            split_string_suspect: true,
            ..
        } => Some(
            "this looks like a single \"...\" string broken \
             apart by an unescaped inner \" — nested double \
             quotes close the outer string. Escape them as \
             \\\" inside the string, or drop the inner quoting"
                .to_string(),
        ),
        TypeErrorKind::CommandNotFunction {
            split_string_suspect: false,
            ..
        } => Some(
            "a command head must be a function or a thunk; \
             a value here is data, not something you can invoke — pass it \
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
            "`within` also takes `handlers:`, written as a list of arms — \
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
        TypeErrorKind::UnboundVariable { name, suggestions } => {
            Some(unbound_variable_hint(name, suggestions))
        }
        TypeErrorKind::HeadBoundToValue { name, .. } => Some(format!(
            "to run the command on PATH instead, write `^{name}`"
        )),
        TypeErrorKind::IndexContainerUnknown { .. } => Some(
            "a function that indexes with a computed key has one container type per program: \
             call it on a list or on a map, or read a list with a literal key, `$xs[0]`"
                .to_string(),
        ),
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
        // Phrased as the runtime phrases its own missing-key hint, so a reader
        // meeting the two errors meets one language.
        TypeErrorKind::RowExtraField { known, .. }
            if !known.is_empty() && !matches!(reason, Some(Reason::RecordUpdate { .. })) => {
            Some(format!(
                "available: {} — did you mean one of those?",
                known.join(", ")
            ))
        }
        TypeErrorKind::DuplicateField { label } => Some(format!(
            "a record has one value per field, so keep whichever '{label}' you meant; \
             to update a field a spread supplies, write it once after the spread, \
             as in `[...$m, {label}: …]`"
        )),
        // The remedy is the shape's own, and the shape's own is where the
        // spawn-time refusal reads it too.
        TypeErrorKind::ExecArgNotText { command, ty } => {
            RefusedArg::of_ty(ty).map(|refusal| refusal.remedy(command))
        }
        TypeErrorKind::FailStatusZero => Some("use `return` for a clean exit".to_string()),
        TypeErrorKind::MalformedAlias { detail } | TypeErrorKind::MalformedUnalias { detail } => {
            Some(detail.to_string())
        }
        TypeErrorKind::IndexIntoThunk => Some(
            "run the block first, then index its result: `!{!$t}[field]` \
             (`!$t[field]` reads `field` off `$t` and forces *that*)"
                .to_string(),
        ),
        TypeErrorKind::FieldOnNonRecord { .. } => {
            Some(
                "check that the value you're indexing is a record like `[a: 1, b: 2]` or a map like `[:, a: 1]`"
                    .to_string(),
            )
        }
        TypeErrorKind::DynamicIndexOnScalar { .. } => Some(
            "only lists (key: Integer) and maps (key: String) \
             accept a key computed at runtime — for a record \
             field, use a static name like $r[fieldname]"
                .to_string(),
        ),
        TypeErrorKind::CaseOnNonVariant { .. } => {
            Some("construct the value with a tag (`name payload) before scrutinising it".to_string())
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
    if matches!(reason, Some(Reason::IfBranches { .. })) && not_a_block(kind) {
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
        Reason::ListPattern => Some(
            "the pattern `[a, b, ...]` only destructures a list — \
             the value being bound has to be a list of the same shape"
                .to_string(),
        ),
        Reason::RecordPattern => Some(
            "the pattern `[key: name, ...]` only destructures a \
             record — the value being bound has to be a record \
             with at least the named fields"
                .to_string(),
        ),
        Reason::Argument => Some(
            "the function's parameter type and the argument's type \
             must agree — check what the function expects and what \
             you're passing in"
                .to_string(),
        ),
        Reason::NotOperand => Some(
            "`not` flips a Bool — its operand has to be a Bool (`true` / `false` or a comparison)"
                .to_string(),
        ),
        Reason::MapKey => {
            Some("map keys must be Strings — quote a bare token or convert with `str`".to_string())
        }
        Reason::ListSpread => list_spread_shape_hint(kind).or_else(|| {
            Some(
                "a `...x` spread copies the elements of a \
                 list into this position, so the value \
                 after `...` must itself be a list"
                    .to_string(),
            )
        }),
        Reason::ListIndexKey => {
            Some("indexing into a list takes an Integer (the position)".to_string())
        }
        Reason::MapIndexKey => Some("indexing into a map takes a String (the key)".to_string()),
        Reason::CaseArmPayload => Some(
            "the `case` arm binds the payload the scrutinee \
             constructs at that tag, so the two must agree"
                .to_string(),
        ),
        Reason::CaseArmHandler => Some(
            "an arm that names its handler runs that handler on the payload, \
             so the name must stand for a function of one argument — to hand \
             a value back instead, write the arm out, as in \
             `ok: { |_| return 5 }"
                .to_string(),
        ),
        Reason::CaseArms { writer } => Some(arm_join_hint(kind, writer.as_deref(), "arm")),
        Reason::ForceOperand => Some(
            "the `!` operator runs a block — its operand must be a \
             block value (something built with `{ ... }`), not data"
                .to_string(),
        ),
        Reason::IfCond => Some(
            "the condition of an `if` must be a Bool — either `true`/`false` \
             or an expression that produces one (e.g. `$[$x == 1]`)"
                .to_string(),
        ),
        Reason::IfBranches { writer } => Some(arm_join_hint(kind, writer.as_deref(), "branch")),
        Reason::TryArms { writer } => Some(arm_join_hint(kind, writer.as_deref(), "outcome")),
        Reason::StandsIn(standing) => Some(stands_in_hint(standing, kind)),
        Reason::PipelineStageWrites { stage, next } => {
            Some(stage_writes_hint(stage.as_deref(), next.as_deref(), kind))
        }
        Reason::BinaryOperands(op) => Some(
            match op {
                BinaryOpKind::Arith(ArithOp::Mod) => {
                    "`%` takes Ints — a float has no remainder here"
                }
                BinaryOpKind::Arith(_) => {
                    "the two sides of a `+` / `-` / `*` / `/` must have the same numeric type"
                }
                BinaryOpKind::Compare(_) => {
                    "you can only compare two values of the same type with `<` / `>` / `<=` / `>=`"
                }
                BinaryOpKind::Eq(_) => {
                    "you can only check equality between two values of the same type"
                }
            }
            .to_string(),
        ),
        Reason::PipelineStageShape => Some(
            "a pipeline stage must be ready to run, not still waiting for an \
             argument — apply it to its argument (`f $x`) rather than piping \
             into it, or read the incoming bytes with a decoder such as \
             `from-line` if it should consume the stream instead"
                .to_string(),
        ),
        Reason::DiscardedValueShape => Some(
            "a discarded statement's value is thrown away, so it must be \
             ready to run, not still waiting for an argument — apply it to \
             what's left, or write `$name` if you meant to keep the function \
             itself"
                .to_string(),
        ),
        Reason::OptionField { form, key } => Some(format!("{form} {key}: wrong value type")),
        Reason::HandlerArm => Some(
            "an arm installed under a name stands in for that command, so it is a block \
             the call runs — `[handlers: [deploy: { |args| … }]]`, or a name bound to one"
                .to_string(),
        ),
        Reason::ListElem => Some(
            "a list is homogeneous — every element has the one type — so this \
             element must be what its neighbours are; give it their shape, or make \
             every element a tagged value and `case` on it downstream"
                .to_string(),
        ),
        Reason::MapElem => Some(
            "a map has computed keys, so every value must have the one type; if the \
             values are genuinely different shapes, write the keys out as labels \
             (`[a: 1, b: \"two\"]`) — that is a record, and a record's fields each \
             keep their own type"
                .to_string(),
        ),
        Reason::RecordUpdate { base } => Some(record_update_hint(base.as_deref(), kind)),
        Reason::MapSpread => Some(
            "a `...x` spread inside a map literal (`[:, …]`) copies another map's \
             entries into it, so the value after `...` must itself be a map — a \
             record's fields are reached by name, so spread it into a record literal \
             (`[...$r, …]`) instead"
                .to_string(),
        ),
        Reason::ScopeBody => Some(
            "`within`, `grant`, `try`, `guard` and `audit` each run a block — the body \
             has to be a block value (something built with `{ ... }`), not data"
                .to_string(),
        ),
        Reason::TryHandler => Some(
            "a `try` handler is given the error the body raised, so it has to be a \
             block of one argument: `try { … } { |e| … }`"
                .to_string(),
        ),
        Reason::ReturnShape => Some(
            "this position needs a computation that is ready to run, and this one is \
             still waiting for an argument — apply it to what's missing (`f $x`), or \
             write `$name` if you meant to hand over the function itself"
                .to_string(),
        ),
        Reason::LetRecSelf => Some(
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
        // These constraints cannot fail on user input, already have a complete
        // diagnosis, or apply only to checker-inserted nodes.
        Reason::CaseScrutinee
        | Reason::Negation
        | Reason::Interpolation
        | Reason::RecordFieldRead
        | Reason::MapKeyRead
        | Reason::DynamicIndexTarget
        | Reason::AutoderefHead
        | Reason::Capture => None,
    }
}

/// A spread updates the fields its record has and adds none, so a label the
/// record lacks gets the misspelling or the three idioms; any other failure
/// is a spread of something that is no record.
fn record_update_hint(base: Option<&str>, kind: &TypeErrorKind) -> String {
    let TypeErrorKind::RowExtraField { label, known } = kind else {
        return "a `...x` spread inside a record literal updates another record's fields, \
                so the value after `...` must itself be a record — a map's keys are data, \
                not labels, so a map has no fields to update"
            .to_string();
    };
    let subject = base.map_or_else(|| "this record".to_string(), |b| format!("`${b}`"));
    let lead = format!(
        "{subject} has no field `{label}` to update — a spread replaces fields its record already has"
    );
    if let [near] = crate::text::near_names(label, known.iter().map(String::as_str), 1)[..] {
        return format!("{lead} (did you mean `{near}`?)");
    }
    let r = base.unwrap_or("r");
    format!(
        "{lead}; to carry a new field, give the record the field from the start, \
         nest it (`[{r}: ${r}, total: …]`), or use a map (`[:, ...${r}, total: …]`)"
    )
}

/// A spread that mismatches because the value is a record, not a list —
/// blaming `[...x]`'s list shape rather than the caller's record. Which side
/// lands in `expected` is an accident of the call site, so both are checked.
fn list_spread_shape_hint(kind: &TypeErrorKind) -> Option<String> {
    let TypeErrorKind::TyMismatch { expected, actual } = kind else {
        return None;
    };
    (matches!(**expected, Ty::Record(_)) || matches!(**actual, Ty::Record(_))).then(|| {
        "this is a list literal, and `...` here copies list elements — a record \
         merge is written as a record literal (`[...$a, port: 1]`), a map merge as a \
         map literal (`[:, ...a, ...b]`)"
            .to_string()
    })
}

/// Reasons where two types meet on equal footing, so a hint read off
/// their shapes beats the reason's own sentence.
fn meets_as_peers(reason: &Reason) -> bool {
    matches!(
        reason,
        Reason::Argument
            | Reason::IfBranches { .. }
            | Reason::CaseArms { .. }
            | Reason::TryArms { .. }
    )
}

/// Help for a `TyMismatch` derived from the types' own shapes, not from which
/// reason raised it — a `Ty::Thunk` parameter, an error-record `[status: Int |
/// r]`, and a byte/list confusion each name what the mismatch *is*, whether it
/// arose at an argument, a spread, or a branch join. Which side lands in
/// `expected` is an accident of the call site, so every test here considers
/// both orders.
fn shape_hint(kind: &TypeErrorKind) -> Option<String> {
    let TypeErrorKind::TyMismatch { expected, actual } = kind else {
        return None;
    };
    let is_record = |t: &Ty| matches!(t, Ty::Record(_));
    let is_map = |t: &Ty| matches!(t, Ty::Map(_));
    if (is_record(expected) && is_map(actual)) || (is_map(expected) && is_record(actual)) {
        let open_record = [expected, actual]
            .into_iter()
            .any(|t| matches!(&**t, Ty::Record(row) if row_is_open(row)));
        let block_clause = if open_record {
            "a bare label on a block's parameter reads a record field; to read a map's key \
             in a block, index it with a computed key, `$m[$k]`. "
        } else {
            ""
        };
        return Some(format!(
            "{block_clause}a record and a map are different types over the same pairs: a \
             record's fields are reached by name (`$r[a]`), while a map's keys are data. If \
             these keys are data, write each literal as a map — `[:, a: 1, b: 2]`, \
             `[$k: v]`, and `[:]` for the empty one, there being no empty-record literal"
        ));
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
             `` `ok 1 ``) and the other is not — tag the plain one, or take the tagged \
             one apart with `case` first"
                .to_string(),
        );
    }
    let is_error_record = |t: &Ty| matches!(t, Ty::Record(row) if row_has_status_int(row));
    if is_error_record(expected) || is_error_record(actual) {
        return Some(
            "a failure is raised with an error record: at least \
             `[status: Int, message: String]` with a nonzero status, and any other \
             fields you care to carry — `fail $e` re-raises a caught error as it stands"
                .to_string(),
        );
    }
    byte_writer_hint(expected, actual)
}

/// Whether a row ends in a variable: a record a read by label left open, not a
/// literal.
fn row_is_open(row: &Row) -> bool {
    match row {
        Row::Extend(_, _, tail) => row_is_open(tail),
        Row::Var(_) => true,
        Row::Empty => false,
    }
}

/// Whether a row carries `status: Int`.  An error carries the row as it stood
/// when the constraint failed, and nothing re-applies it here, so a `Var` tail
/// reads as absent.  That only ever withholds a hint, never asserts one.
fn row_has_status_int(row: &Row) -> bool {
    let mut rest = row;
    loop {
        match rest {
            Row::Extend(label, ty, tail) => {
                if *label == Label::Field("status".into()) {
                    return **ty == Ty::Int;
                }
                rest = tail;
            }
            Row::Empty | Row::Var(_) => return false,
        }
    }
}

/// A list where `Bytes` was wanted, or the reverse.  Every byte-channel writer
/// takes one argument type, so the hint must hold for each of them.
fn byte_writer_hint(expected: &Ty, actual: &Ty) -> Option<String> {
    let list_for_bytes = |a: &Ty, b: &Ty| matches!(a, Ty::List(_)) && matches!(b, Ty::Bytes);
    (list_for_bytes(expected, actual) || list_for_bytes(actual, expected)).then(|| {
        "a Bytes value and a list are different things: `from-bytes` yields Bytes \
         and `to-bytes` writes it back; a list of numbers is written by \
         `ints-to-bytes`, a list of lines by `to-lines`"
            .to_string()
    })
}

/// Why `$STATUS` names nothing; the runtime's undefined-variable error says
/// the same.
pub(crate) const NO_STATUS_REGISTER: &str = "there is no status register: a failure raises an \
    error record that carries its own status — catch it with `try` and read \
    `$err[status]` from the handler's argument";

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
        return format!("environment variables are read as `$ENV[{name}]`");
    }
    "check the spelling, or define it with `let` before this line".into()
}

/// What a contract file that returns no record should return instead.
fn return_not_record_hint(form: &str, found: &Ty, offered: &[&str]) -> String {
    let key = offered.first().copied().unwrap_or("key");
    match found {
        Ty::Map(_) => format!(
            "the keys of `{form}` are its labels, so write a record — `[{key}: …]`, \
             not `[:, {key}: …]`"
        ),
        Ty::List(_) => format!(
            "`[]` is the empty list, not the empty record; a file with nothing to set \
             returns `()`, and otherwise `{form}` wants a record such as `[{key}: …]`"
        ),
        Ty::Thunk(_) => format!("`{form}` returns its settings, not a block"),
        _ => format!("return a record such as `[{key}: …]`, or `()` for nothing to set"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::ty::CompTy;

    fn mismatch(expected: Ty, actual: Ty) -> TypeErrorKind {
        TypeErrorKind::TyMismatch {
            expected: Box::new(expected),
            actual: Box::new(actual),
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
}
