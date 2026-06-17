//! Type schemes (`forall alpha. A`) and type errors.
//!
//! A `Scheme` is a type with universally quantified variables — the result
//! of generalisation at `let` bindings.  Instantiation replaces quantified
//! variables with fresh unification variables at each use site, giving
//! let-polymorphism.
//!
//! `TypeError` and `TypeErrorKind` represent the diagnostics produced by
//! unification and inference failures.

use super::fmt::{FmtCtx, fmt_mode_ctx, fmt_ty_ctx};
use super::ty::{CompTy, CompTyVar, ModeVar, PipeMode, RowVar, Ty, TyVar};
use crate::source::Span;
use std::collections::BTreeSet;

// ─────────────────────────────────────────────────────────────────────────────
// Type scheme:  ∀α₁…αₙ ∀ρ₁…ρₖ ∀μ₁…μₘ. A
// ─────────────────────────────────────────────────────────────────────────────

/// Cached residual free variables for a scheme — those free in the scheme's
/// type that were NOT quantified because they appeared in the environment at
/// generalisation time.  For fully-generalised (top-level) schemes all three
/// sets are empty.
///
/// Stored on generalised schemes so that `env_free_vars` can skip a full
/// type-tree traversal and read the cached sets directly.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CachedFreeVars {
    pub ty_fv: BTreeSet<TyVar>,
    #[serde(default)]
    pub comp_fv: BTreeSet<CompTyVar>,
    pub mode_fv: BTreeSet<ModeVar>,
    pub row_fv: BTreeSet<RowVar>,
}

/// A polymorphic type scheme: `forall alpha_1 ... alpha_n, rho_1 ... rho_k, mu_1 ... mu_m. A`.
///
/// Quantifies over three variable kinds simultaneously: value types, row
/// types, and pipeline modes.  `ty` is the body of the scheme — the type
/// under the quantifiers.
///
/// Recursive types — both computation and value — are captured by
/// `comp_ty_bindings` and `ty_bindings`: snapshots of `(old_root,
/// applied_binding)` pairs for every var that is part of a cycle in the
/// scheme's body.  At instantiation time each entry is given a fresh var
/// id and re-bound to the binding with substitutions applied, so two
/// instantiations of the same scheme do not share a union-find slot for
/// the cycle root.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Scheme {
    pub ty_vars: Vec<TyVar>,
    /// Quantified non-cyclic comp-type variables.  `instantiate` mints
    /// fresh ids for each entry so polymorphic schemes whose body
    /// contains a free comp var (e.g. `Thunk(γ)` for an unconstrained
    /// γ) do not share that var across use sites.
    #[serde(default)]
    pub comp_ty_vars: Vec<CompTyVar>,
    pub mode_vars: Vec<ModeVar>,
    pub row_vars: Vec<RowVar>,
    pub ty: Ty,
    /// Snapshotted cyclic comp-var bindings (key: original root id).
    /// Empty for non-recursive schemes.  Generalisation populates this
    /// from the unifier's union-find; instantiation re-binds fresh ids
    /// to the substituted bindings.
    #[serde(default)]
    pub comp_ty_bindings: Vec<(u32, CompTy)>,
    /// Snapshotted cyclic ty-var bindings (key: original root id).
    /// Mirror of `comp_ty_bindings` for value-type cycles such as the
    /// streaming-consumer α := Variant {`more {head, tail: Thunk(α)},
    /// `done | ρ}.
    #[serde(default)]
    pub ty_bindings: Vec<(u32, Ty)>,
    /// Pre-computed residual free variables.  `None` for monomorphic schemes
    /// whose free variables change as unification proceeds.  `Some` for
    /// schemes produced by `generalize()` or for fully-closed builtins.
    pub cached_fv: Option<CachedFreeVars>,
}

impl Scheme {
    /// A monomorphic scheme: no quantified variables.
    pub fn mono(ty: Ty) -> Self {
        Scheme {
            ty_vars: vec![],
            comp_ty_vars: vec![],
            mode_vars: vec![],
            row_vars: vec![],
            ty,
            comp_ty_bindings: vec![],
            ty_bindings: vec![],
            cached_fv: None,
        }
    }
    /// True when the scheme quantifies over at least one variable.
    pub fn is_poly(&self) -> bool {
        !self.ty_vars.is_empty()
            || !self.comp_ty_vars.is_empty()
            || !self.mode_vars.is_empty()
            || !self.row_vars.is_empty()
            || !self.comp_ty_bindings.is_empty()
            || !self.ty_bindings.is_empty()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Type errors
// ─────────────────────────────────────────────────────────────────────────────

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
    /// Command head is a non-callable value (e.g. a literal `String` in
    /// command position with arguments).  Reported under the same code as
    /// `CompTyMismatch` (T0011) — it is the same condition, framed in
    /// surface terms instead of as a `Cmd a vs a → b` mismatch.
    CommandNotCallable {
        ty: Ty,
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
    /// Free-form message from the inferencer, not from the unifier.
    AdHoc {
        message: String,
    },
}

impl TypeErrorKind {
    /// Stable per-phase error code (`T####`).
    pub fn code(&self) -> &'static str {
        match self {
            TypeErrorKind::RecursiveRow => "T0002",
            TypeErrorKind::TypeTooDeep => "T0003",
            TypeErrorKind::TyMismatch { .. } => "T0010",
            TypeErrorKind::CompTyMismatch { .. } => "T0011",
            TypeErrorKind::CommandNotCallable { .. } => "T0011",
            TypeErrorKind::ModeMismatch { .. } => "T0012",
            TypeErrorKind::RowExtraField { .. } => "T0020",
            TypeErrorKind::RowMissingField { .. } => "T0021",
            TypeErrorKind::CaseNotExhaustive { .. } => "T0030",
            TypeErrorKind::CaseLabelTypeMismatch { .. } => "T0031",
            TypeErrorKind::AdHoc { .. } => "T0000",
        }
    }

    /// Render a single-line diagnostic message.
    ///
    /// Phrasing is intentionally symmetric where the surface mistake
    /// admits no canonical "expected vs got" reading.  The orientation of
    /// `expected`/`actual` inside the unifier depends on which call site
    /// fires the constraint; for a beginner the more honest framing is
    /// "these two types must agree but don't".  GHC uses the same shape:
    /// `Couldn't match type ‘Int’ with ‘String’`.
    pub fn render_message(&self) -> String {
        match self {
            TypeErrorKind::RecursiveRow => {
                "infinite row — a record's field list would refer back to itself".into()
            }
            TypeErrorKind::TypeTooDeep => "type nesting exceeds the supported depth".into(),
            TypeErrorKind::TyMismatch { expected, actual } => {
                let ctx = FmtCtx::for_value_types(&[expected, actual]);
                format!(
                    "couldn't match type {} with type {}",
                    fmt_ty_ctx(expected, &ctx),
                    fmt_ty_ctx(actual, &ctx)
                )
            }
            TypeErrorKind::CompTyMismatch { diffs, .. } => fmt_comp_mismatch(diffs),
            TypeErrorKind::ModeMismatch { expected, actual } => {
                let ctx = FmtCtx::default();
                format!(
                    "pipeline channels don't agree: one side is {}, the other is {}",
                    fmt_mode_ctx(expected, &ctx),
                    fmt_mode_ctx(actual, &ctx)
                )
            }
            TypeErrorKind::RowExtraField { label } => {
                format!("this record has no field named '{label}'")
            }
            TypeErrorKind::RowMissingField { label } => {
                format!("this record is missing a field named '{label}'")
            }
            TypeErrorKind::CommandNotCallable { ty } => {
                let ctx = FmtCtx::for_value_types(&[ty]);
                format!(
                    "value of type {} cannot be used as a command head",
                    fmt_ty_ctx(ty, &ctx)
                )
            }
            TypeErrorKind::CaseNotExhaustive { missing, extra } => {
                fmt_case_exhaustiveness(missing, extra)
            }
            TypeErrorKind::CaseLabelTypeMismatch {
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
            TypeErrorKind::AdHoc { message } => message.clone(),
        }
    }
}

/// A located type error: source span, structural cause, and optional hint.
#[derive(Debug, Clone)]
pub struct TypeError {
    pub pos: Option<Span>,
    pub kind: TypeErrorKind,
    pub hint: Option<String>,
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

/// Format a `CompTyMismatch` in user-friendly prose, suppressing the
/// internal `Cmd α → β` shape when the difference reduces to a return
/// type or a single channel.  When `diffs` is empty the two head shapes
/// (Return vs Fun, etc.) failed to unify without identifying a specific
/// component, and this returns a generic note that one computation is a
/// function and the other is not.
fn fmt_comp_mismatch(diffs: &[CompDiff]) -> String {
    use CompDiff::*;
    if diffs.is_empty() {
        return "two computations have incompatible shapes — one is a function, the other is not"
            .into();
    }
    // Build one shared FmtCtx over every type/mode mentioned by any
    // diff so the same variable prints with the same Greek letter on
    // both sides of every line.
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
            ctx.absorb_mode(expected);
            ctx.absorb_mode(actual);
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

/// Format a `CaseNotExhaustive` set of missing/extra labels.  Singletons
/// and plurals get separate phrasings — "no handler for `err" reads
/// better than "missing handlers for `err".
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
