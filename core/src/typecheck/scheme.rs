//! Type schemes (`forall alpha. A`) and type errors.
//!
//! A `Scheme` is a type with universally quantified variables — the result
//! of generalisation at `let` bindings.  Instantiation replaces quantified
//! variables with fresh unification variables at each use site, giving
//! let-polymorphism.
//!
//! `TypeError` and `TypeErrorKind` represent the diagnostics produced by
//! unification and inference failures.

use super::fmt::{fmt_comp_ty, fmt_mode, fmt_ty};
use super::ty::{CompTy, CompTyVar, ModeVar, PipeMode, RowVar, Ty, TyVar};
use crate::span::Span;
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
/// Recursive computation types are captured by `comp_ty_bindings`:
/// a snapshot of `(old_root, applied_binding)` pairs for every comp var
/// that is part of a cycle in the scheme's body.  At instantiation time
/// each entry is given a fresh comp var id and re-bound to the binding
/// with substitutions applied, so two instantiations of the same
/// scheme do not share a union-find slot for the cycle root.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    RecursiveType,
    RecursiveRow,
    RecursiveCompTy,
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
    HeadNotCallable {
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
            TypeErrorKind::RecursiveType => "T0001",
            TypeErrorKind::RecursiveRow => "T0002",
            TypeErrorKind::RecursiveCompTy => "T0003",
            TypeErrorKind::TyMismatch { .. } => "T0010",
            TypeErrorKind::CompTyMismatch { .. } => "T0011",
            TypeErrorKind::HeadNotCallable { .. } => "T0011",
            TypeErrorKind::ModeMismatch { .. } => "T0012",
            TypeErrorKind::RowExtraField { .. } => "T0020",
            TypeErrorKind::RowMissingField { .. } => "T0021",
            TypeErrorKind::CaseNotExhaustive { .. } => "T0030",
            TypeErrorKind::CaseLabelTypeMismatch { .. } => "T0031",
            TypeErrorKind::AdHoc { .. } => "T0000",
        }
    }

    /// Render a single-line diagnostic message.
    pub fn render_message(&self) -> String {
        match self {
            TypeErrorKind::RecursiveType => "recursive type".into(),
            TypeErrorKind::RecursiveRow => "recursive row type".into(),
            TypeErrorKind::RecursiveCompTy => "recursive computation type".into(),
            TypeErrorKind::TyMismatch { expected, actual } => {
                format!("type mismatch: {} vs {}", fmt_ty(expected), fmt_ty(actual))
            }
            TypeErrorKind::CompTyMismatch {
                expected,
                actual,
                diffs,
            } => {
                let head = format!(
                    "command type mismatch: {} vs {}",
                    fmt_comp_ty(expected),
                    fmt_comp_ty(actual)
                );
                if diffs.is_empty() {
                    head
                } else {
                    let body = diffs
                        .iter()
                        .map(|d| match d {
                            CompDiff::Stdin { expected, actual } => {
                                format!("  stdin: {} vs {}", fmt_mode(expected), fmt_mode(actual))
                            }
                            CompDiff::Stdout { expected, actual } => {
                                format!("  stdout: {} vs {}", fmt_mode(expected), fmt_mode(actual))
                            }
                            CompDiff::ReturnType { expected, actual } => {
                                format!("  return type: {} vs {}", fmt_ty(expected), fmt_ty(actual))
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("{head}\n{body}")
                }
            }
            TypeErrorKind::ModeMismatch { expected, actual } => format!(
                "pipeline mode mismatch: {} vs {}",
                fmt_mode(expected),
                fmt_mode(actual)
            ),
            TypeErrorKind::RowExtraField { label } => {
                format!("record has unexpected field '{label}'")
            }
            TypeErrorKind::RowMissingField { label } => {
                format!("record is missing field '{label}'")
            }
            TypeErrorKind::HeadNotCallable { ty } => {
                format!("value of type {} cannot be used as a command head", fmt_ty(ty))
            }
            TypeErrorKind::CaseNotExhaustive { missing, extra } => {
                let mut parts = Vec::new();
                if !missing.is_empty() {
                    parts.push(format!("missing handlers for {}", missing.join(", ")));
                }
                if !extra.is_empty() {
                    parts.push(format!("extra handlers for {}", extra.join(", ")));
                }
                format!("case is not exhaustive: {}", parts.join("; "))
            }
            TypeErrorKind::CaseLabelTypeMismatch {
                label,
                expected,
                found,
            } => format!(
                "case handler at {label} has wrong shape: expected {}, got {}",
                fmt_ty(expected),
                fmt_ty(found)
            ),
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
