//! Display helpers for types in error messages and `:type` output.
//!
//! Pure functions over the type algebra — they do not call into the unifier
//! and do not modify any state.  Each `fmt_*` function renders a type as a
//! human-readable string.  `fmt_scheme` handles the quantifier prefix and
//! assigns Greek-letter names to quantified variables.
//!
//! Diagnostic-side rendering goes through [`FmtCtx::for_value_types`] or one
//! of its siblings: it walks the type(s) you're about to print, mints a
//! Greek letter for every distinct free unification variable in
//! first-appearance order, and gives back a context you pass to
//! [`fmt_ty_ctx`].  The same variable then prints as the same letter
//! everywhere it appears, on both sides of a `couldn't match` message —
//! GHC's "rigid type variable" trick, minus the rigidity.

use super::scheme::Scheme;
use super::ty::{CompTy, CompTyVar, ModeVar, PipeMode, Row, RowVar, Ty, TyVar};
use std::collections::HashMap;

/// Greek-letter alphabets used to name unification variables in
/// diagnostics.  Picked to match GHC and the HM literature: lower-case
/// Greek for value types, `ϕ χ ψ ω` for computation types (suspended
/// commands), `μ ν ξ π` for pipeline modes (Greek 'pipe' adjacent
/// letters), `ρ σ τ υ` for row tails.  Cycle through the alphabet by
/// appending an integer when we run out of fresh letters.
const TY_LETTERS: &[&str] = &["α", "β", "γ", "δ", "ε", "ζ", "η", "θ", "ι", "κ"];
const COMP_LETTERS: &[&str] = &["ϕ", "χ", "ψ", "ω"];
const MODE_LETTERS: &[&str] = &["μ", "ν", "ξ", "π"];
const ROW_LETTERS: &[&str] = &["ρ", "σ", "τ", "υ"];

fn pick(letters: &[&str], idx: usize) -> String {
    if idx < letters.len() {
        letters[idx].to_string()
    } else {
        // After exhausting the alphabet, subscript with an index:
        // α, β, …, κ, α1, β1, … — visually clear that they're still
        // type variables, not new symbols.
        format!("{}{}", letters[idx % letters.len()], idx / letters.len())
    }
}

/// Formatting context: maps unification variables to display names.
///
/// Variables not in the map fall back to placeholders (`_` for types,
/// `...` for rows) — appropriate for `:type` output where the user is
/// looking at *one* type and the variable identity isn't load-bearing.
/// Diagnostics that mention multiple types should always pre-populate
/// via [`FmtCtx::for_value_types`] or a sibling so the same variable
/// prints the same way on both sides.
#[derive(Default)]
pub struct FmtCtx {
    pub ty_names: HashMap<TyVar, String>,
    pub comp_names: HashMap<CompTyVar, String>,
    pub mode_names: HashMap<ModeVar, String>,
    pub row_names: HashMap<RowVar, String>,
}

impl FmtCtx {
    fn ty_name(&self, v: TyVar) -> String {
        self.ty_names.get(&v).cloned().unwrap_or_else(|| "_".into())
    }
    fn row_name(&self, v: RowVar) -> Option<String> {
        self.row_names.get(&v).cloned()
    }
    fn comp_name(&self, v: CompTyVar) -> String {
        self.comp_names
            .get(&v)
            .cloned()
            .unwrap_or_else(|| "_".into())
    }
    fn mode_name(&self, v: ModeVar) -> Option<String> {
        self.mode_names.get(&v).cloned()
    }

    /// Build a context that names every free unification variable
    /// appearing in the given types, in first-appearance order.  The
    /// caller passes whichever types will be rendered side-by-side so
    /// shared variables get one consistent name — the same trick GHC
    /// uses to give matching pairs of types coordinated variable names
    /// in `Couldn't match type ‘a’ with ‘b’` errors.
    pub fn for_value_types(types: &[&Ty]) -> Self {
        let mut ctx = Self::default();
        for t in types {
            ctx.absorb_ty(t);
        }
        ctx
    }

    /// Walk `ty` and assign a Greek letter to every unification variable
    /// (value, computation, mode, row) we haven't seen yet.  Insertion
    /// order is preserved, so the first variable we encounter prints as
    /// α (or its cousin in the comp/mode/row alphabets).
    pub(super) fn absorb_ty(&mut self, ty: &Ty) {
        match ty {
            Ty::Var(v) => {
                if !self.ty_names.contains_key(v) {
                    let idx = self.ty_names.len();
                    self.ty_names.insert(*v, pick(TY_LETTERS, idx));
                }
            }
            Ty::List(a) | Ty::Map(a) | Ty::Handle(a) => self.absorb_ty(a),
            Ty::Record(r) | Ty::Variant(r) => self.absorb_row(r),
            Ty::Thunk(b) => self.absorb_comp(b),
            Ty::Unit | Ty::Bytes | Ty::Bool | Ty::Int | Ty::Float | Ty::String => {}
        }
    }

    pub(super) fn absorb_comp(&mut self, cty: &CompTy) {
        match cty {
            CompTy::Var(v) => {
                if !self.comp_names.contains_key(v) {
                    let idx = self.comp_names.len();
                    self.comp_names.insert(*v, pick(COMP_LETTERS, idx));
                }
            }
            CompTy::Return(spec, a) => {
                self.absorb_mode(spec.input);
                self.absorb_mode(spec.output);
                self.absorb_ty(a);
            }
            CompTy::Fun(a, b) => {
                self.absorb_ty(a);
                self.absorb_comp(b);
            }
        }
    }

    fn absorb_row(&mut self, row: &Row) {
        match row {
            Row::Empty => {}
            Row::Var(v) => {
                if !self.row_names.contains_key(v) {
                    let idx = self.row_names.len();
                    self.row_names.insert(*v, pick(ROW_LETTERS, idx));
                }
            }
            Row::Extend(_, ty, rest) => {
                self.absorb_ty(ty);
                self.absorb_row(rest);
            }
        }
    }

    pub(super) fn absorb_mode(&mut self, mode: PipeMode) {
        if let PipeMode::Var(v) = mode
            && !self.mode_names.contains_key(&v)
        {
            let idx = self.mode_names.len();
            self.mode_names.insert(v, pick(MODE_LETTERS, idx));
        }
    }
}

pub fn fmt_ty(ty: &Ty) -> String {
    fmt_ty_ctx(ty, &FmtCtx::default())
}

pub fn fmt_ty_ctx(ty: &Ty, ctx: &FmtCtx) -> String {
    match ty {
        Ty::Unit => "Unit".into(),
        Ty::Bytes => "Bytes".into(),
        Ty::Bool => "Bool".into(),
        Ty::Int => "Integer".into(),
        Ty::Float => "Float".into(),
        Ty::String => "String".into(),
        Ty::Handle(a) => format!("Handle {}", fmt_ty_ctx(a, ctx)),
        Ty::Var(v) => ctx.ty_name(*v),
        Ty::List(a) => format!("[{}]", fmt_ty_ctx(a, ctx)),
        Ty::Map(a) => format!("[String:{}]", fmt_ty_ctx(a, ctx)),
        Ty::Record(r) => format!("[{}]", fmt_row_ctx(r, ctx)),
        Ty::Variant(r) => format!("[{}]", fmt_variant_row_ctx(r, ctx)),
        Ty::Thunk(b) => format!("{{{}}}", fmt_comp_ty_ctx(b, ctx)),
    }
}

/// Like [`fmt_row_ctx`] but with `|` separators — the surface convention for
/// variant rows, distinguishing them from tag-keyed records (which use `,`).
pub fn fmt_variant_row_ctx(row: &Row, ctx: &FmtCtx) -> String {
    fmt_row_with_sep(row, ctx, " | ")
}

pub fn fmt_row_ctx(row: &Row, ctx: &FmtCtx) -> String {
    fmt_row_with_sep(row, ctx, ", ")
}

/// Shared body for record/variant row rendering.  `sep` is the
/// separator between field/arm entries; an open tail (`Row::Var`) prints
/// as ` ...` (or ` ...ρ` when the variable has a name in `ctx`) so the
/// tail is visually distinct from the labelled fields — `[a: Int, b:
/// String, ...]` reads as "two known fields, possibly more", which is
/// what an open row means.
fn fmt_row_with_sep(row: &Row, ctx: &FmtCtx, sep: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut tail: Option<String> = None;
    let mut cur = row;
    loop {
        match cur {
            Row::Empty => break,
            Row::Var(v) => {
                tail = Some(match ctx.row_name(*v) {
                    Some(name) => format!("...{name}"),
                    None => "...".into(),
                });
                break;
            }
            Row::Extend(l, ty, rest) => {
                // Under scoped-label semantics the first occurrence of a label
                // is the visible one; shadowed duplicates are not shown.
                if seen.insert(l.as_str()) {
                    parts.push(format!("{l}: {}", fmt_ty_ctx(ty, ctx)));
                }
                cur = rest;
            }
        }
    }
    match (parts.is_empty(), tail) {
        (true, None) => String::new(),
        (true, Some(t)) => t,
        (false, None) => parts.join(sep),
        (false, Some(t)) => format!("{}{sep}{}", parts.join(sep), t),
    }
}

pub fn fmt_comp_ty_ctx(cty: &CompTy, ctx: &FmtCtx) -> String {
    match cty {
        CompTy::Var(v) => ctx.comp_name(*v),
        CompTy::Fun(a, b) => format!("{} → {}", fmt_ty_ctx(a, ctx), fmt_comp_ty_ctx(b, ctx)),
        CompTy::Return(spec, a) => {
            let mut fields: Vec<String> = Vec::new();
            if let Some(s) = fmt_mode_field_ctx(spec.input) {
                fields.push(format!("stdin: {s}"));
            }
            if let Some(s) = fmt_mode_field_ctx(spec.output) {
                fields.push(format!("stdout: {s}"));
            }
            if fields.is_empty() {
                format!("Command {}", fmt_ty_ctx(a, ctx))
            } else {
                format!("Command[{}] {}", fields.join(", "), fmt_ty_ctx(a, ctx))
            }
        }
    }
}

fn fmt_mode_field_ctx(mode: PipeMode) -> Option<String> {
    match mode {
        PipeMode::None | PipeMode::Var(_) => None,
        PipeMode::Bytes => Some("Bytes".into()),
    }
}

/// Format a pipeline mode for standalone display (e.g. in error messages).
pub fn fmt_mode(mode: &PipeMode) -> String {
    fmt_mode_ctx(mode, &FmtCtx::default())
}

/// Like [`fmt_mode`] but consults `ctx` for a friendly name on
/// unbound mode variables.
///
/// When the variable has been pre-named by
/// [`FmtCtx::for_value_types`], it prints as `μ`/`ν`/…;
/// otherwise it falls back to `_`.
pub fn fmt_mode_ctx(mode: &PipeMode, ctx: &FmtCtx) -> String {
    match mode {
        PipeMode::None => "(no channel)".into(),
        PipeMode::Bytes => "Bytes".into(),
        PipeMode::Var(v) => ctx.mode_name(*v).unwrap_or_else(|| "_".into()),
    }
}

/// Format a type scheme with proper quantifier prefix and named variables.
///
/// Type variables are assigned Greek letters (α, β, γ, …); computation-type
/// variables get ϕ, χ, ψ, ω, …; mode variables get μ, ν, ξ, π, …; row
/// variables get ρ, σ, τ, …  The body strips the outer `Thunk` wrapper so that
/// the displayed form is a `Cmd` type rather than `{Cmd …}`.
pub fn fmt_scheme(scheme: &Scheme) -> String {
    let mut ty_order: Vec<TyVar> = scheme.ty_vars.clone();
    for (root, _) in &scheme.ty_bindings {
        let v = TyVar(*root);
        if !ty_order.contains(&v) {
            ty_order.push(v);
        }
    }
    let ty_names: HashMap<TyVar, String> = ty_order
        .iter()
        .enumerate()
        .map(|(i, v)| (*v, pick(TY_LETTERS, i)))
        .collect();
    let mode_names: HashMap<ModeVar, String> = scheme
        .mode_vars
        .iter()
        .enumerate()
        .map(|(i, v)| (*v, pick(MODE_LETTERS, i)))
        .collect();
    let row_names: HashMap<RowVar, String> = scheme
        .row_vars
        .iter()
        .enumerate()
        .map(|(i, v)| (*v, pick(ROW_LETTERS, i)))
        .collect();
    let mut comp_order: Vec<CompTyVar> = scheme.comp_ty_vars.clone();
    for (root, _) in &scheme.comp_ty_bindings {
        let v = CompTyVar(*root);
        if !comp_order.contains(&v) {
            comp_order.push(v);
        }
    }
    let comp_names: HashMap<CompTyVar, String> = comp_order
        .iter()
        .enumerate()
        .map(|(i, v)| (*v, pick(COMP_LETTERS, i)))
        .collect();

    let ctx = FmtCtx {
        ty_names,
        comp_names,
        mode_names,
        row_names,
    };

    let quant_parts: Vec<String> = ty_order
        .iter()
        .map(|v| ctx.ty_names[v].clone())
        .chain(comp_order.iter().map(|v| ctx.comp_names[v].clone()))
        .chain(scheme.mode_vars.iter().map(|v| ctx.mode_names[v].clone()))
        .chain(scheme.row_vars.iter().map(|v| ctx.row_names[v].clone()))
        .collect();

    let prefix = if quant_parts.is_empty() {
        String::new()
    } else {
        format!("∀{}. ", quant_parts.join(" "))
    };

    // Strip the outer Thunk wrapper produced by the `thunk(...)` helper.
    let body = match &scheme.ty {
        Ty::Thunk(cty) => fmt_comp_ty_ctx(cty, &ctx),
        other => fmt_ty_ctx(other, &ctx),
    };

    format!("{prefix}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_scheme_quantifies_cyclic_ty_roots() {
        let root = TyVar(17);
        let scheme = Scheme {
            ty_vars: vec![],
            comp_ty_vars: vec![],
            mode_vars: vec![],
            row_vars: vec![],
            ty: Ty::List(Box::new(Ty::Var(root))),
            comp_ty_bindings: vec![],
            ty_bindings: vec![(root.0, Ty::List(Box::new(Ty::Var(root))))],
            cached_fv: None,
        };
        let rendered = fmt_scheme(&scheme);
        assert_eq!(rendered, "∀α. [α]");
    }
}
