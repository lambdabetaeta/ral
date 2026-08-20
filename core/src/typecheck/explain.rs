//! User-facing prose for type errors.  `infer.rs` and `unify.rs` raise them
//! as data; every sentence a user reads is a pure function of that data,
//! written here and nowhere else.

use super::error::{CompDiff, Reason, SpreadHead, TypeErrorKind};
use super::fmt::{FmtCtx, fmt_route_ctx, fmt_ty_ctx};
use super::ty::Ty;
use crate::serial::plural;
use crate::syntax::ast::BinaryOpKind;
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
            Self::TyMismatch { expected, actual } => {
                let ctx = FmtCtx::for_value_types(&[expected, actual]);
                format!(
                    "couldn't match type {} with type {}",
                    fmt_ty_ctx(expected, &ctx),
                    fmt_ty_ctx(actual, &ctx)
                )
            }
            Self::CompTyMismatch { diffs, .. } => fmt_comp_mismatch(diffs),
            Self::RouteMismatch { expected, actual } => {
                let ctx = FmtCtx::default();
                format!(
                    "these two computations disagree about where their payload lives: \
                     one is {}, the other is {}",
                    fmt_route_ctx(expected, &ctx),
                    fmt_route_ctx(actual, &ctx)
                )
            }
            Self::RowExtraField { label, .. } => {
                format!("this record has no field named '{label}'")
            }
            Self::RowMissingField { label } => {
                format!("this record is missing a field named '{label}'")
            }
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
            Self::ExecArgNotText { command, ty } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!(
                    "cannot pass {} to external command '{command}'",
                    fmt_ty_ctx(ty, &ctx)
                )
            }
            Self::FailStatusZero => {
                "`fail [status: 0]` is not allowed — fail requires a nonzero status".into()
            }
            Self::ErrorRecordMessage { actual } => {
                let ctx = FmtCtx::for_value_types(&[actual]);
                format!(
                    "an error record's `message` must be a String or Bytes, but this one is {}",
                    fmt_ty_ctx(actual, &ctx)
                )
            }
            Self::MalformedAlias { .. } => "malformed alias definition".into(),
            Self::MalformedUnalias { .. } => "malformed unalias".into(),
            Self::IndexIntoThunk => {
                "this is a block — you can't read a field from it directly".into()
            }
            Self::FieldOnNonRecord { label, ty } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!(
                    "you tried to read the field `{label}` from a value of type {}, but only records have fields",
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
            Self::TyMismatch { expected, actual } => {
                let ctx = FmtCtx::for_value_types(&[expected, actual]);
                format!(
                    "{} doesn't match {}",
                    fmt_ty_ctx(expected, &ctx),
                    fmt_ty_ctx(actual, &ctx)
                )
            }
            Self::CompTyMismatch { .. } => "types disagree here".into(),
            Self::CommandNotFunction { ty, .. } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!("{} cannot be invoked as a command", fmt_ty_ctx(ty, &ctx))
            }
            Self::RouteMismatch { .. } => "these disagree about where their payload lives".into(),
            Self::RowExtraField { label, .. } => format!("no field '{label}' in this record"),
            Self::RowMissingField { label } => format!("this record needs field '{label}'"),
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
            Self::ErrorRecordMessage { .. } => "this `message` is not text".into(),
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
        }
    }
}

/// Prose for a `CompTyMismatch`.  `unify.rs` leaves `diffs` empty only when
/// the two heads differ in shape — `Return` against `Fun` — blaming no
/// component.
fn fmt_comp_mismatch(diffs: &[CompDiff]) -> String {
    use CompDiff::{ReturnType, Route};
    if diffs.is_empty() {
        return "two computations have incompatible shapes — one is a function, the other is not"
            .into();
    }
    // One context over every diff, so a variable keeps its letter throughout.
    let ty_refs: Vec<&Ty> = diffs
        .iter()
        .flat_map(|d| match d {
            ReturnType { expected, actual } => vec![expected, actual],
            Route { .. } => Vec::new(),
        })
        .collect();
    let mut ctx = FmtCtx::for_value_types(&ty_refs);
    for d in diffs {
        if let Route { expected, actual } = d {
            ctx.absorb_route(*expected);
            ctx.absorb_route(*actual);
        }
    }

    let only_return_type = diffs.iter().all(|d| matches!(d, ReturnType { .. }));
    if only_return_type {
        let parts: Vec<String> = diffs
            .iter()
            .filter_map(|d| match d {
                ReturnType { expected, actual } => Some(format!(
                    "couldn't match type {} with type {}",
                    fmt_ty_ctx(expected, &ctx),
                    fmt_ty_ctx(actual, &ctx)
                )),
                Route { .. } => None,
            })
            .collect();
        return parts.join("; ");
    }
    let mut lines: Vec<String> = Vec::with_capacity(diffs.len() + 1);
    lines.push("these two computations don't line up:".into());
    for d in diffs {
        let line = match d {
            Route { expected, actual } => format!(
                "  payload route: one is {}, the other is {}",
                fmt_route_ctx(expected, &ctx),
                fmt_route_ctx(actual, &ctx)
            ),
            ReturnType { expected, actual } => format!(
                "  return type: couldn't match {} with {}",
                fmt_ty_ctx(expected, &ctx),
                fmt_ty_ctx(actual, &ctx)
            ),
        };
        lines.push(line);
    }
    lines.join("\n")
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

/// The guidance sentence: keyed on the kind where the kind is its own complete
/// diagnosis, otherwise on the [`Reason`] the failed constraint was raised
/// under.  That second match is wildcard-free on purpose, so a new `Reason`
/// must be given prose or listed as hintless.
pub(super) fn hint(kind: &TypeErrorKind, reason: Option<&Reason>) -> Option<String> {
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
        // A `case` arm's head is the handler it named, and the `Reason` match
        // below explains it as such.
        TypeErrorKind::CommandNotFunction {
            split_string_suspect: false,
            ..
        } if !matches!(reason, Some(Reason::CaseArmHandler)) => Some(
            "a command head must be a function or a thunk; \
             a value here is data, not something you can invoke — pass it \
             as an argument, or wrap it in a function instead"
                .to_string(),
        ),
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
        TypeErrorKind::RowExtraField { known, .. } if !known.is_empty() => Some(format!(
            "available: {} — did you mean one of those?",
            known.join(", ")
        )),
        // The remedy is the shape's own, and the shape's own is where the
        // spawn-time refusal reads it too.
        TypeErrorKind::ExecArgNotText { command, ty } => {
            RefusedArg::of_ty(ty).map(|refusal| refusal.remedy(command))
        }
        TypeErrorKind::FailStatusZero => Some("use `return` for a clean exit".to_string()),
        TypeErrorKind::ErrorRecordMessage { .. } => Some(
            "the message is the text the failure carries — render the value first, \
             as in `message: \"$[to-string $v]\"`"
                .to_string(),
        ),
        TypeErrorKind::MalformedAlias { detail } | TypeErrorKind::MalformedUnalias { detail } => {
            Some(detail.to_string())
        }
        TypeErrorKind::IndexIntoThunk => Some(
            "run the block first, then index its result: `!{!$t}[field]` \
             (`!$t[field]` reads `field` off `$t` and forces *that*)"
                .to_string(),
        ),
        TypeErrorKind::FieldOnNonRecord { .. } => {
            Some("check that the value you're indexing is a record like `[a: 1, b: 2]`".to_string())
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
        Reason::BuiltinBlockArg => Some("this builtin expects a block value here".to_string()),
        Reason::NotOperand => Some(
            "`not` flips a Bool — its operand has to be a Bool (`true` / `false` or a comparison)"
                .to_string(),
        ),
        Reason::MapKey => {
            Some("map keys must be Strings — quote a bare token or convert with `str`".to_string())
        }
        Reason::ListSpread => Some(
            "a `...x` spread copies the elements of a \
             list into this position, so the value \
             after `...` must itself be a list"
                .to_string(),
        ),
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
        Reason::CaseArms => Some(
            "every arm of a `case` must agree on where its payload lives: \
             either every arm returns a value of the same type, or every arm \
             is captured from stdout — a mix of the two cannot join, so pipe \
             the stdout-routed arm through a decoder (`| from-string`) to \
             bring it onto the value side"
                .to_string(),
        ),
        Reason::CaseArmValues => Some(
            "every arm of a `case` returns a value and exactly one arm runs, \
             so the `case` has a single type that every arm must produce — \
             convert the odd arm to that type, or have every arm return a \
             tagged value and `case` on it downstream"
                .to_string(),
        ),
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
        Reason::IfBranches => Some(
            "both branches of an `if` must agree on where their payload lives: \
             either both return a value of the same type, or both are \
             captured from stdout — a mix of the two cannot join, so pipe \
             the stdout-routed branch through a decoder (`| from-string`) to \
             bring it onto the value side"
                .to_string(),
        ),
        Reason::IfBranchValues => Some(
            "both branches of an `if` return a value and exactly one of them \
             runs, so the `if` has a single type that both branches must \
             produce — convert one branch to the other's type, or have both \
             return a tagged value and `case` on it downstream"
                .to_string(),
        ),
        Reason::ChainBranches => Some(
            "every arm of a `?` chain must agree on where their payload lives: \
             either every arm returns a value of the same type, or every arm \
             is captured from stdout — a mix of the two cannot join, so pipe \
             the stdout-routed arm through a decoder (`| from-string`) to \
             bring it onto the value side"
                .to_string(),
        ),
        Reason::ChainBranchValues => Some(
            "every arm of a `?` chain returns a value and the chain yields \
             whichever arm succeeds, so the chain has a single type that every \
             arm must produce — convert the odd arm to that type, or have every \
             arm return a tagged value and `case` on it downstream"
                .to_string(),
        ),
        Reason::TryArms => Some(
            "both outcomes of a `try` must agree on where their payload lives: \
             either both return a value of the same type, or both are \
             captured from stdout — a mix of the two cannot join, so pipe \
             the stdout-routed outcome through a decoder (`| from-string`) to \
             bring it onto the value side"
                .to_string(),
        ),
        Reason::TryArmValues => Some(
            "both outcomes of a `try` return a value and exactly one of them \
             happens, so the `try` has a single type that the body and the \
             handler must both produce — convert one to the other's type, or \
             have both return a tagged value and `case` on it downstream"
                .to_string(),
        ),
        Reason::RoutePin => Some(
            "this computation's payload route was still undecided, so an \
             earlier use pinned it to where its payload turned out to live — \
             a later use expecting the other route disagrees with that pin"
                .to_string(),
        ),
        Reason::BinaryOperands(op) => Some(
            match op {
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
        Reason::OptionField { form, key } => Some(format!("{form} {key}: wrong value type")),
        Reason::ErrorRecordArg => Some(
            "a failure is raised with an error record: at least `[status: Int]` with a \
             nonzero status, optionally a `message` of String or Bytes, and any other \
             fields you care to carry — `fail $e` re-raises a caught error as it stands"
                .to_string(),
        ),
        // The same pin fails two different ways.  A route clash is about
        // *where* the payload lives; a WF-2 failure is about the arm having a
        // returned value at all, and telling that author to "add a codec"
        // would send them after the wrong thing.
        Reason::HandlerRoutePin => Some(match kind {
            TypeErrorKind::CompTyMismatch { diffs, .. }
                if diffs
                    .iter()
                    .any(|d| matches!(d, CompDiff::ReturnType { .. })) =>
            {
                "this head's payload is its stdout, so an arm installed under it has \
                 no separate value to return — its return type must be Unit. Write the \
                 arm to emit its result rather than return it, or install it under a \
                 head whose payload is a returned value"
                    .to_string()
            }
            _ => "a handler or alias reinterprets a head — it preserves the head's \
                  payload route; match the existing head's route or add a codec"
                .to_string(),
        }),
        Reason::AliasParam
        | Reason::BuiltinTypedArg
        | Reason::ReturnShape
        | Reason::TryHandler
        | Reason::ScopeBody
        | Reason::CaseScrutinee
        | Reason::ListElem
        | Reason::MapElem
        | Reason::MapSpread
        | Reason::RecordFieldRead
        | Reason::DynamicIndexTarget
        | Reason::AutoderefHead
        | Reason::LetRecSelf
        | Reason::LinesStepSelf
        | Reason::CaptureOperand
        | Reason::DecodeOperand => None,
    }
}
