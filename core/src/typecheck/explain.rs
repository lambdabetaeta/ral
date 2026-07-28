//! User-facing prose for type errors.  `infer.rs` and `unify.rs` raise them
//! as data; every sentence a user reads is a pure function of that data,
//! written here and nowhere else.

use super::error::{CompDiff, Reason, TypeErrorKind};
use super::fmt::{FmtCtx, fmt_mode_ctx, fmt_ty_ctx};
use super::ty::Ty;
use crate::syntax::ast::BinaryOpKind;

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
            Self::ModeMismatch { expected, actual } => {
                let ctx = FmtCtx::default();
                format!(
                    "pipeline channels don't agree: one side is {}, the other is {}",
                    fmt_mode_ctx(expected, &ctx),
                    fmt_mode_ctx(actual, &ctx)
                )
            }
            Self::RowExtraField { label } => {
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
            Self::CaseLabelTypeMismatch {
                label,
                expected,
                found,
            } => {
                let ctx = FmtCtx::for_value_types(&[expected, found]);
                format!(
                    "the handler for {label} has the wrong shape — it should be a function taking {}, but it has type {}",
                    fmt_ty_ctx(expected, &ctx),
                    fmt_ty_ctx(found, &ctx)
                )
            }
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
            Self::CannotRedefineBuiltin { name, verb } => {
                format!("cannot {verb} builtin `{name}`")
            }
            Self::HandlerShadowedByBinding { name } => {
                format!("handler `{name}` is hidden by a lexical binding in this scope")
            }
            Self::BuiltinArity {
                expected,
                got,
                at_most: false,
            } => format!("builtin expected {expected} argument(s), got {got}"),
            Self::BuiltinArity {
                expected,
                got,
                at_most: true,
            } => format!("builtin expected at most {expected} argument(s), got {got}"),
            Self::DecoderTakesNoArgument { name } => {
                format!("`{name}` takes no argument — it reads the byte channel")
            }
            Self::FailStatusZero => {
                "`fail [status: 0]` is not allowed — fail requires a nonzero status".into()
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
            Self::ModeMismatch { .. } => "pipeline channels disagree here".into(),
            Self::RowExtraField { label } => format!("no field '{label}' in this record"),
            Self::RowMissingField { label } => format!("this record needs field '{label}'"),
            Self::CaseNotExhaustive { missing, extra } => {
                match (missing.as_slice(), extra.as_slice()) {
                    ([only], []) => format!("no handler for {only}"),
                    (some, []) => format!("no handler for {}", some.join(", ")),
                    ([], [only]) => format!("handler for {only} that the value never produces"),
                    ([], some) => format!(
                        "handlers for {} that the value never produces",
                        some.join(", ")
                    ),
                    _ => "case alternatives don't match the value".into(),
                }
            }
            Self::CaseLabelTypeMismatch { label, .. } => {
                format!("the handler at {label} is the wrong shape")
            }
            Self::CaseOnNonVariant { .. }
            | Self::ControlOperatorAsValue { .. }
            | Self::HandlerNotFirstClass { .. }
            | Self::BuiltinNotFirstClass { .. }
            | Self::CannotRedefineBuiltin { .. }
            | Self::HandlerShadowedByBinding { .. }
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
    use CompDiff::{ReturnType, Stdin, Stdout};
    if diffs.is_empty() {
        return "two computations have incompatible shapes — one is a function, the other is not"
            .into();
    }
    // One context over every diff, so a variable keeps its letter throughout.
    let ty_refs: Vec<&Ty> = diffs
        .iter()
        .flat_map(|d| match d {
            ReturnType { expected, actual } => vec![expected, actual],
            _ => Vec::new(),
        })
        .collect();
    let mut ctx = FmtCtx::for_value_types(&ty_refs);
    for d in diffs {
        if let Stdin { expected, actual } | Stdout { expected, actual } = d {
            ctx.absorb_mode(*expected);
            ctx.absorb_mode(*actual);
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
                _ => None,
            })
            .collect();
        return parts.join("; ");
    }
    let mut lines: Vec<String> = Vec::with_capacity(diffs.len() + 1);
    lines.push("these two computations don't line up:".into());
    for d in diffs {
        let line = match d {
            Stdin { expected, actual } => format!(
                "  stdin channel: one expects {}, the other {}",
                fmt_mode_ctx(expected, &ctx),
                fmt_mode_ctx(actual, &ctx)
            ),
            Stdout { expected, actual } => format!(
                "  stdout channel: one expects {}, the other {}",
                fmt_mode_ctx(expected, &ctx),
                fmt_mode_ctx(actual, &ctx)
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
        [one] => parts.push(format!("no handler for {one}")),
        many => parts.push(format!("no handlers for {}", many.join(", "))),
    }
    match extra {
        [] => {}
        [one] => parts.push(format!("handler for {one} but the value never produces it")),
        many => parts.push(format!(
            "handlers for {} but the value never produces them",
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
        TypeErrorKind::CommandNotFunction {
            split_string_suspect: false,
            ..
        } => Some(
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
        TypeErrorKind::CannotRedefineBuiltin { .. } => Some(
            "lexical and builtin names are not handler names; did you mean `let name = ...`?"
                .to_string(),
        ),
        TypeErrorKind::HandlerShadowedByBinding { .. } => Some(
            "bare command lookup resolves to the lexical value before handlers are considered"
                .to_string(),
        ),
        TypeErrorKind::BuiltinArity { at_most: false, .. } => {
            Some("check the builtin's help entry for its command shape".to_string())
        }
        TypeErrorKind::BuiltinArity { at_most: true, .. } => {
            Some("remove the extra arguments or pass a single list value".to_string())
        }
        TypeErrorKind::DecoderTakesNoArgument { .. } => Some(
            "to decode a value in hand, pipe it through the matching encoder: \
             `to-string $x | from-json`"
                .to_string(),
        ),
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
        Reason::AliasArgv => Some(
            "this argument is passed to an alias/handler arm — its \
             type must match what the arm's body does with the argv \
             elements"
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
            "the `case` arm's handler must accept the payload \
             type the scrutinee constructs at that tag"
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
            "both branches of an `if` must produce the same type, \
             because the whole expression has one type"
                .to_string(),
        ),
        Reason::TryArms => Some(
            "both outcomes of a `try` must produce the same value when observed; \
             if one arm only writes a line, that line counts as the value at the boundary"
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
        Reason::PipedValue { step_stream } => {
            let mut hint = String::from(
                "this stage produces a value that is piped into the next \
                 stage's function — the value's type and the function's \
                 parameter type must agree",
            );
            if *step_stream {
                hint.push_str(
                    "; this stage receives a lazy Step stream — consume it \
                     explicitly with stream-each / stream-map / stream-to-list",
                );
            }
            Some(hint)
        }
        Reason::OptionField { form, key } => Some(format!("{form} {key}: wrong value type")),
        Reason::HandlerModePin => Some(
            "a handler or alias reinterprets a head — it preserves the head's \
             pipeline modes; match the existing head's modes or add a codec"
                .to_string(),
        ),
        Reason::AliasParam
        | Reason::BuiltinTypedArg
        | Reason::PipelineEdge
        | Reason::ReturnShape
        | Reason::TryHandler
        | Reason::ScopeBody
        | Reason::CaseScrutinee
        | Reason::CaseTable
        | Reason::ListElem
        | Reason::MapElem
        | Reason::MapSpread
        | Reason::RecordFieldRead
        | Reason::DynamicIndexTarget
        | Reason::AutoderefHead
        | Reason::TypeProbe
        | Reason::LetRecSelf
        | Reason::LinesStepSelf => None,
    }
}
