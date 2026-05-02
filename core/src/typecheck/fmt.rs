//! Display helpers for types in error messages and `:type` output.
//!
//! Pure functions over the type algebra — they do not call into the unifier
//! and do not modify any state.  Each `fmt_*` function renders a type as a
//! human-readable string.  `fmt_scheme` handles the quantifier prefix and
//! assigns Greek-letter names to quantified variables.

use super::scheme::Scheme;
use super::ty::{CompTy, CompTyVar, ModeVar, PipeMode, Row, RowVar, Ty, TyVar};
use std::collections::HashMap;

/// Formatting context: maps quantified variables to human-readable names
/// (Greek letters).  A default context renders all variables as `_`.
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
    fn row_name(&self, v: RowVar) -> String {
        self.row_names
            .get(&v)
            .cloned()
            .unwrap_or_else(|| "..".into())
    }
    fn comp_name(&self, v: CompTyVar) -> String {
        self.comp_names.get(&v).cloned().unwrap_or_else(|| "_".into())
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
        Ty::Int => "Int".into(),
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
    let mut parts: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cur = row;
    loop {
        match cur {
            Row::Empty => return parts.join(" | "),
            Row::Var(v) => {
                parts.push(ctx.row_name(*v));
                return parts.join(" | ");
            }
            Row::Extend(l, ty, rest) => {
                if seen.insert(l.as_str()) {
                    parts.push(format!("{}: {}", l, fmt_ty_ctx(ty, ctx)));
                }
                cur = rest;
            }
        }
    }
}

pub fn fmt_row_ctx(row: &Row, ctx: &FmtCtx) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cur = row;
    loop {
        match cur {
            Row::Empty => return parts.join(", "),
            Row::Var(v) => {
                parts.push(ctx.row_name(*v));
                return parts.join(", ");
            }
            Row::Extend(l, ty, rest) => {
                // Under scoped-label semantics the first occurrence of a label
                // is the visible one; shadowed duplicates are not shown.
                if seen.insert(l.as_str()) {
                    parts.push(format!("{}: {}", l, fmt_ty_ctx(ty, ctx)));
                }
                cur = rest;
            }
        }
    }
}

pub fn fmt_comp_ty(cty: &CompTy) -> String {
    fmt_comp_ty_ctx(cty, &FmtCtx::default())
}

pub fn fmt_comp_ty_ctx(cty: &CompTy, ctx: &FmtCtx) -> String {
    match cty {
        CompTy::Var(v) => ctx.comp_name(*v),
        CompTy::Fun(a, b) => format!("{} → {}", fmt_ty_ctx(a, ctx), fmt_comp_ty_ctx(b, ctx)),
        CompTy::Return(spec, a) => {
            let mut fields: Vec<String> = Vec::new();
            if let Some(s) = fmt_mode_field_ctx(&spec.input, ctx) {
                fields.push(format!("stdin: {}", s));
            }
            if let Some(s) = fmt_mode_field_ctx(&spec.output, ctx) {
                fields.push(format!("stdout: {}", s));
            }
            if fields.is_empty() {
                format!("Cmd {}", fmt_ty_ctx(a, ctx))
            } else {
                format!("Cmd[{}] {}", fields.join(", "), fmt_ty_ctx(a, ctx))
            }
        }
    }
}

fn fmt_mode_field_ctx(mode: &PipeMode, _ctx: &FmtCtx) -> Option<String> {
    match mode {
        PipeMode::None | PipeMode::Var(_) => None,
        PipeMode::Bytes => Some("Bytes".into()),
    }
}

/// Format a pipeline mode for standalone display (e.g. in error messages).
pub fn fmt_mode(mode: &PipeMode) -> String {
    match mode {
        PipeMode::None => "none".into(),
        PipeMode::Bytes => "Bytes".into(),
        PipeMode::Var(_) => "_".into(),
    }
}

/// Format a type scheme with proper quantifier prefix and named variables.
///
/// Type variables are assigned Greek letters (α, β, γ, …); computation-type
/// variables get ϕ, χ, ψ, ω, …; mode variables get μ, ν, ξ, π, …; row
/// variables get ρ, σ, τ, …  The body strips the outer `Thunk` wrapper so that
/// the displayed form is a `Cmd` type rather than `{Cmd …}`.
pub fn fmt_scheme(scheme: &Scheme) -> String {
    const TY_NAMES: &[&str] = &["α", "β", "γ", "δ", "ε", "ζ", "η", "θ", "ι", "κ"];
    const COMP_NAMES: &[&str] = &["ϕ", "χ", "ψ", "ω"];
    const MODE_NAMES: &[&str] = &["μ", "ν", "ξ", "π"];
    const ROW_NAMES: &[&str] = &["ρ", "σ", "τ", "υ"];

    let ty_names: HashMap<TyVar, String> = scheme
        .ty_vars
        .iter()
        .enumerate()
        .map(|(i, v)| (*v, TY_NAMES[i % TY_NAMES.len()].to_string()))
        .collect();
    let mode_names: HashMap<ModeVar, String> = scheme
        .mode_vars
        .iter()
        .enumerate()
        .map(|(i, v)| (*v, MODE_NAMES[i % MODE_NAMES.len()].to_string()))
        .collect();
    let row_names: HashMap<RowVar, String> = scheme
        .row_vars
        .iter()
        .enumerate()
        .map(|(i, v)| (*v, ROW_NAMES[i % ROW_NAMES.len()].to_string()))
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
        .map(|(i, v)| (*v, COMP_NAMES[i % COMP_NAMES.len()].to_string()))
        .collect();

    let ctx = FmtCtx {
        ty_names,
        comp_names,
        mode_names,
        row_names,
    };

    let quant_parts: Vec<String> = scheme
        .ty_vars
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

    format!("{}{}", prefix, body)
}
