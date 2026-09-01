//! Rendering types as text, for error messages and `:type` output.
//!
//! Pure functions over the type algebra; nothing here consults the unifier.
//! A diagnostic that prints two types must give a shared variable the same
//! name in both, so callers first build one [`FmtCtx`] over everything they
//! are about to render: it mints Greek letters in first-appearance order.

use super::scheme::Scheme;
use super::ty::{CompTy, CompTyVar, PayloadRoute, PayloadVar, Row, RowVar, Ty, TyVar};
use std::collections::HashMap;

// One alphabet per kind of unification variable, kept disjoint so a letter
// alone tells the reader which kind it names.
const TY_LETTERS: &[&str] = &["α", "β", "γ", "δ", "ε", "ζ", "η", "θ", "ι", "κ"];
const COMP_LETTERS: &[&str] = &["ϕ", "χ", "ψ", "ω"];
const ROUTE_LETTERS: &[&str] = &["μ", "ν", "ξ", "π"];
const ROW_LETTERS: &[&str] = &["ρ", "σ", "τ", "υ"];

fn pick(letters: &[&str], idx: usize) -> String {
    if idx < letters.len() {
        letters[idx].to_string()
    } else {
        format!("{}{}", letters[idx % letters.len()], idx / letters.len())
    }
}

/// Display names for unification variables.
///
/// A variable absent from the map prints as a placeholder (`_`, or `...` for
/// a row tail) — right for `:type` on a single type, wrong for a diagnostic
/// naming two, which must go through [`FmtCtx::for_value_types`].
#[derive(Default)]
pub struct FmtCtx {
    pub ty_names: HashMap<TyVar, String>,
    pub comp_names: HashMap<CompTyVar, String>,
    pub route_names: HashMap<PayloadVar, String>,
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
    fn route_name(&self, v: PayloadVar) -> Option<String> {
        self.route_names.get(&v).cloned()
    }

    /// Name every unification variable in `types`, in first-appearance order.
    /// Pass every type that will be rendered side by side, so a variable they
    /// share gets one name.
    pub fn for_value_types(types: &[&Ty]) -> Self {
        let mut ctx = Self::default();
        for t in types {
            ctx.absorb_ty(t);
        }
        ctx
    }

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
            CompTy::Return(route, a) => {
                self.absorb_route(*route);
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

    pub(super) fn absorb_route(&mut self, route: PayloadRoute) {
        if let PayloadRoute::Var(v) = route
            && !self.route_names.contains_key(&v)
        {
            let idx = self.route_names.len();
            self.route_names.insert(v, pick(ROUTE_LETTERS, idx));
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

/// Variant rows use `|` between arms and a backtick on every tag, including an
/// open tail. Records and variants both render inside `[…]`.
pub fn fmt_variant_row_ctx(row: &Row, ctx: &FmtCtx) -> String {
    fmt_row_with_sep(row, ctx, " | ", "`")
}

pub fn fmt_row_ctx(row: &Row, ctx: &FmtCtx) -> String {
    fmt_row_with_sep(row, ctx, ", ", "")
}

/// Shared body for record and variant rows. `tail_sigil` marks an open tail as
/// belonging to that row kind; a named tail appends its row variable.
fn fmt_row_with_sep(row: &Row, ctx: &FmtCtx, sep: &str, tail_sigil: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut tail: Option<String> = None;
    let mut cur = row;
    loop {
        match cur {
            Row::Empty => break,
            Row::Var(v) => {
                tail = Some(match ctx.row_name(*v) {
                    Some(name) => format!("{tail_sigil}...{name}"),
                    None => format!("{tail_sigil}..."),
                });
                break;
            }
            Row::Extend(l, ty, rest) => {
                // Row unification walks the spine head-first and matches the
                // first occurrence of a label, so show only that one.
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
        CompTy::Return(PayloadRoute::Bytes, _) => "Command captured from stdout".into(),
        CompTy::Return(_, a) => format!("Command {}", fmt_ty_ctx(a, ctx)),
    }
}

/// Format a payload route on its own, outside any type — the mismatch
/// renderer's one use, since a route otherwise prints as nothing inside a
/// `Command` type (see [`fmt_comp_ty_ctx`]).
pub fn fmt_route(route: &PayloadRoute) -> String {
    fmt_route_ctx(route, &FmtCtx::default())
}

/// Like [`fmt_route`], but a route variable takes its name from `ctx` when it
/// has one there.
pub fn fmt_route_ctx(route: &PayloadRoute, ctx: &FmtCtx) -> String {
    match route {
        PayloadRoute::Value => "a returned value".into(),
        PayloadRoute::Bytes => "captured from stdout".into(),
        PayloadRoute::Var(v) => ctx.route_name(*v).unwrap_or_else(|| "_".into()),
    }
}

fn names_in_order<V: Copy + Eq + std::hash::Hash>(
    order: &[V],
    letters: &[&str],
) -> HashMap<V, String> {
    order
        .iter()
        .enumerate()
        .map(|(i, v)| (*v, pick(letters, i)))
        .collect()
}

/// Format a scheme with its ∀ prefix, naming variables by their position in
/// the scheme's quantifier lists.
///
/// Route variables are named but never quantified: an open route prints as
/// nothing inside a `Command` type, so its binder would dangle.  The outer
/// `Thunk` is stripped, so a command reads `Command …`, not `{Command …}`.
pub fn fmt_scheme(scheme: &Scheme) -> String {
    // Roots of cyclic bindings are quantified too, after the plain vars.
    let mut ty_order: Vec<TyVar> = scheme.ty_vars.clone();
    for (root, _) in &scheme.ty_bindings {
        let v = TyVar(*root);
        if !ty_order.contains(&v) {
            ty_order.push(v);
        }
    }
    let mut comp_order: Vec<CompTyVar> = scheme.comp_ty_vars.clone();
    for (root, _) in &scheme.comp_ty_bindings {
        let v = CompTyVar(*root);
        if !comp_order.contains(&v) {
            comp_order.push(v);
        }
    }

    let ctx = FmtCtx {
        ty_names: names_in_order(&ty_order, TY_LETTERS),
        comp_names: names_in_order(&comp_order, COMP_LETTERS),
        route_names: names_in_order(&scheme.route_vars, ROUTE_LETTERS),
        row_names: names_in_order(&scheme.row_vars, ROW_LETTERS),
    };

    let quant_parts: Vec<String> = ty_order
        .iter()
        .map(|v| ctx.ty_names[v].clone())
        .chain(comp_order.iter().map(|v| ctx.comp_names[v].clone()))
        .chain(scheme.row_vars.iter().map(|v| ctx.row_names[v].clone()))
        .collect();

    let prefix = if quant_parts.is_empty() {
        String::new()
    } else {
        format!("∀{}. ", quant_parts.join(" "))
    };

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
            route_vars: vec![],
            row_vars: vec![],
            ty: Ty::List(Box::new(Ty::Var(root))),
            comp_ty_bindings: vec![],
            ty_bindings: vec![(root.0, Ty::List(Box::new(Ty::Var(root))))],
            cached_fv: None,
        };
        let rendered = fmt_scheme(&scheme);
        assert_eq!(rendered, "∀α. [α]");
    }

    #[test]
    fn fmt_variant_row_tails_have_a_backtick() {
        let row_var = RowVar(17);
        let variant = Ty::Variant(Row::Var(row_var));
        let record = Ty::Record(Row::Var(row_var));

        assert_eq!(fmt_ty(&variant), "[`...]");
        assert_eq!(fmt_ty(&record), "[...]");

        let ctx = FmtCtx::for_value_types(&[&variant]);
        assert_eq!(fmt_ty_ctx(&variant, &ctx), "[`...ρ]");
        assert_eq!(fmt_ty_ctx(&record, &ctx), "[...ρ]");
    }
}
