//! Rendering types as text, for error messages and `:type` output.
//!
//! Pure functions over the type algebra; nothing here consults the unifier.
//! A diagnostic that prints two types must give a shared variable the same
//! name in both, so callers first build one [`FmtCtx`] over everything they
//! are about to render: it mints Greek letters in first-appearance order.

use super::scheme::{Scheme, WeakVars};
use super::ty::{CompTy, CompTyVar, Grade, GradeVar, Row, RowVar, Ty, TyVar};
use std::collections::{HashMap, HashSet};

// One alphabet per kind of unification variable, kept disjoint so a letter
// alone tells the reader which kind it names.
const TY_LETTERS: &[&str] = &["α", "β", "γ", "δ", "ε", "ζ", "η", "θ", "ι", "κ"];
const COMP_LETTERS: &[&str] = &["ϕ", "χ", "ψ", "ω"];
const ROW_LETTERS: &[&str] = &["ρ", "σ", "τ", "υ"];
const GRADE_LETTERS: &[&str] = &["ν", "ξ", "ο", "π"];

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
#[allow(clippy::struct_field_names)] // one map per variable sort; the sort is the prefix.
#[derive(Default)]
pub struct FmtCtx {
    pub(crate) ty_names: HashMap<TyVar, String>,
    pub(crate) comp_names: HashMap<CompTyVar, String>,
    pub(crate) row_names: HashMap<RowVar, String>,
    pub(crate) grade_names: HashMap<GradeVar, String>,
    cycles: Cycles,
}

/// Cyclic roots with their bindings: a back-edge `Var(root)` stands for its
/// binding, so the types over them are regular trees.
#[derive(Default)]
struct Cycles {
    tys: Vec<(TyVar, Ty)>,
    comps: Vec<(CompTyVar, CompTy)>,
}

/// Node pairs already assumed equal.  Bisimilarity is a greatest fixed point
/// and every obligation is a conjunct, so an assumption is sound; the graphs
/// are finite, so the walk ends.
#[derive(Default)]
struct Assumed {
    tys: HashSet<(*const Ty, *const Ty)>,
    comps: HashSet<(*const CompTy, *const CompTy)>,
}

impl Cycles {
    /// The root whose binding is the same regular tree as `ty`.
    fn ty_root(&self, ty: &Ty) -> Option<&(TyVar, Ty)> {
        self.tys
            .iter()
            .find(|(_, binding)| self.ty_eq(ty, binding, &mut Assumed::default()))
    }

    fn comp_root(&self, cty: &CompTy) -> Option<&(CompTyVar, CompTy)> {
        self.comps
            .iter()
            .find(|(_, binding)| self.comp_eq(cty, binding, &mut Assumed::default()))
    }

    fn ty_binding(&self, v: TyVar) -> Option<&Ty> {
        self.tys.iter().find(|(root, _)| *root == v).map(|(_, b)| b)
    }

    fn comp_binding(&self, v: CompTyVar) -> Option<&CompTy> {
        self.comps
            .iter()
            .find(|(root, _)| *root == v)
            .map(|(_, b)| b)
    }

    fn ty_eq(&self, a: &Ty, b: &Ty, seen: &mut Assumed) -> bool {
        if !seen.tys.insert((a, b)) {
            return true;
        }
        match (a, b) {
            (Ty::Var(v), _) if let Some(a) = self.ty_binding(*v) => self.ty_eq(a, b, seen),
            (_, Ty::Var(v)) if let Some(b) = self.ty_binding(*v) => self.ty_eq(a, b, seen),
            (Ty::List(x), Ty::List(y))
            | (Ty::Map(x), Ty::Map(y))
            | (Ty::Handle(x), Ty::Handle(y)) => self.ty_eq(x, y, seen),
            (Ty::Record(x), Ty::Record(y)) | (Ty::Variant(x), Ty::Variant(y)) => {
                self.row_eq(x, y, seen)
            }
            (Ty::Thunk(x), Ty::Thunk(y)) => self.comp_eq(x, y, seen),
            _ => a == b,
        }
    }

    fn comp_eq(&self, a: &CompTy, b: &CompTy, seen: &mut Assumed) -> bool {
        if !seen.comps.insert((a, b)) {
            return true;
        }
        match (a, b) {
            (CompTy::Var(v), _) if let Some(a) = self.comp_binding(*v) => self.comp_eq(a, b, seen),
            (_, CompTy::Var(v)) if let Some(b) = self.comp_binding(*v) => self.comp_eq(a, b, seen),
            (CompTy::Return(g, x), CompTy::Return(h, y)) => g == h && self.ty_eq(x, y, seen),
            (CompTy::Fun(p, x), CompTy::Fun(q, y)) => {
                self.ty_eq(p, q, seen) && self.comp_eq(x, y, seen)
            }
            _ => a == b,
        }
    }

    fn row_eq(&self, a: &Row, b: &Row, seen: &mut Assumed) -> bool {
        match (a, b) {
            (Row::Extend(l, f, x), Row::Extend(m, g, y)) => {
                l == m && self.ty_eq(f, g, seen) && self.row_eq(x, y, seen)
            }
            _ => a == b,
        }
    }
}

/// A cyclic root whose `μ` encloses the node being printed.
#[derive(Clone, Copy, PartialEq)]
enum Open {
    Ty(TyVar),
    Comp(CompTyVar),
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
    fn grade_name(&self, v: GradeVar) -> String {
        self.grade_names
            .get(&v)
            .cloned()
            .unwrap_or_else(|| "_".into())
    }

    /// A weak variable is named after the quantified ones, with a leading `_`:
    /// it is one type for the whole unit, not a binder.
    fn name_weak(&mut self, weak: &WeakVars) {
        for v in weak.tys.keys() {
            let name = format!("_{}", pick(TY_LETTERS, self.ty_names.len()));
            self.ty_names.insert(*v, name);
        }
        for v in &weak.comps {
            let name = format!("_{}", pick(COMP_LETTERS, self.comp_names.len()));
            self.comp_names.insert(*v, name);
        }
        for v in weak.rows.keys() {
            let name = format!("_{}", pick(ROW_LETTERS, self.row_names.len()));
            self.row_names.insert(*v, name);
        }
    }

    /// Name every unification variable in `types`, in first-appearance order.
    /// Pass every type that will be rendered side by side, so a variable they
    /// share gets one name.
    pub(crate) fn for_value_types(types: &[&Ty]) -> Self {
        let mut ctx = Self::default();
        for t in types {
            ctx.absorb_ty(t);
        }
        ctx
    }

    /// [`Self::for_value_types`], for computations.
    pub(crate) fn for_comp_types(types: &[&CompTy]) -> Self {
        let mut ctx = Self::default();
        for t in types {
            ctx.absorb_comp(t);
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
            CompTy::Return(grade, a) => {
                if let Grade::Var(v) = grade
                    && !self.grade_names.contains_key(v)
                {
                    let idx = self.grade_names.len();
                    self.grade_names.insert(*v, pick(GRADE_LETTERS, idx));
                }
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
}

pub fn fmt_ty(ty: &Ty) -> String {
    fmt_ty_ctx(ty, &FmtCtx::default())
}

pub fn fmt_ty_ctx(ty: &Ty, ctx: &FmtCtx) -> String {
    fmt_ty_in(ty, ctx, &mut Vec::new())
}

/// A node equal to a cyclic root prints as the root's name inside its `μ`,
/// and as its `μ` outside.
fn fmt_ty_in(ty: &Ty, ctx: &FmtCtx, open: &mut Vec<Open>) -> String {
    let Some(&(root, ref binding)) = ctx.cycles.ty_root(ty) else {
        return fmt_ty_node(ty, ctx, open);
    };
    let name = ctx.ty_name(root);
    if open.contains(&Open::Ty(root)) {
        return name;
    }
    open.push(Open::Ty(root));
    let body = fmt_ty_node(binding, ctx, open);
    open.pop();
    format!("μ{name}. {body}")
}

fn fmt_ty_node(ty: &Ty, ctx: &FmtCtx, open: &mut Vec<Open>) -> String {
    match ty {
        Ty::Unit => "Unit".into(),
        Ty::Bytes => "Bytes".into(),
        Ty::Bool => "Bool".into(),
        Ty::Int => "Integer".into(),
        Ty::Float => "Float".into(),
        Ty::String => "String".into(),
        Ty::Handle(a) => format!("Handle {}", fmt_ty_in(a, ctx, open)),
        Ty::Var(v) => ctx.ty_name(*v),
        Ty::List(a) => format!("[{}]", fmt_ty_in(a, ctx, open)),
        Ty::Map(a) => format!("Map {}", fmt_ty_in(a, ctx, open)),
        Ty::Record(r) => format!("[{}]", fmt_row_in(r, ctx, open)),
        Ty::Variant(r) => format!("[{}]", fmt_variant_row_in(r, ctx, open)),
        Ty::Thunk(b) => format!("{{{}}}", fmt_comp_ty_in(b, ctx, open)),
    }
}

/// Variant rows use `|` between arms and a backtick on every tag, including an
/// open tail. Records and variants both render inside `[…]`.
fn fmt_variant_row_in(row: &Row, ctx: &FmtCtx, open: &mut Vec<Open>) -> String {
    fmt_row_with_sep(row, ctx, open, " | ", "`")
}

fn fmt_row_in(row: &Row, ctx: &FmtCtx, open: &mut Vec<Open>) -> String {
    fmt_row_with_sep(row, ctx, open, ", ", "")
}

/// Shared body for record and variant rows. `tail_sigil` marks an open tail as
/// belonging to that row kind; a named tail appends its row variable.
fn fmt_row_with_sep(
    row: &Row,
    ctx: &FmtCtx,
    open: &mut Vec<Open>,
    sep: &str,
    tail_sigil: &str,
) -> String {
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
                if seen.insert(l.clone()) {
                    parts.push(format!("{l}: {}", fmt_ty_in(ty, ctx, open)));
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
    fmt_comp_ty_in(cty, ctx, &mut Vec::new())
}

/// [`fmt_ty_in`] for a computation.
fn fmt_comp_ty_in(cty: &CompTy, ctx: &FmtCtx, open: &mut Vec<Open>) -> String {
    let Some(&(root, ref binding)) = ctx.cycles.comp_root(cty) else {
        return fmt_comp_ty_node(cty, ctx, open);
    };
    let name = ctx.comp_name(root);
    if open.contains(&Open::Comp(root)) {
        return name;
    }
    open.push(Open::Comp(root));
    let body = fmt_comp_ty_node(binding, ctx, open);
    open.pop();
    format!("μ{name}. {body}")
}

fn fmt_comp_ty_node(cty: &CompTy, ctx: &FmtCtx, open: &mut Vec<Open>) -> String {
    match cty {
        CompTy::Var(v) => ctx.comp_name(*v),
        CompTy::Fun(a, b) => format!(
            "{} → {}",
            fmt_ty_in(a, ctx, open),
            fmt_comp_ty_in(b, ctx, open)
        ),
        CompTy::Return(Grade::Output, _) => "Command".into(),
        CompTy::Return(Grade::Value, a) => format!("Returns {}", fmt_ty_in(a, ctx, open)),
        CompTy::Return(Grade::Var(v), a) => {
            format!("{} {}", ctx.grade_name(*v), fmt_ty_in(a, ctx, open))
        }
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
/// The outer `Thunk` is stripped, so a command reads `Command`, not `{Command}`.
pub fn fmt_scheme(scheme: &Scheme) -> String {
    // Roots of cyclic bindings are named after the plain vars; they bind by `μ`, not `∀`.
    let mut ty_order: Vec<TyVar> = scheme.ty_vars.iter().map(|&(v, _)| v).collect();
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

    let mut ctx = FmtCtx {
        ty_names: names_in_order(&ty_order, TY_LETTERS),
        comp_names: names_in_order(&comp_order, COMP_LETTERS),
        row_names: names_in_order(
            &scheme.row_vars.iter().map(|&(v, _)| v).collect::<Vec<_>>(),
            ROW_LETTERS,
        ),
        grade_names: names_in_order(&scheme.grade_vars, GRADE_LETTERS),
        cycles: Cycles {
            tys: scheme
                .ty_bindings
                .iter()
                .map(|(root, binding)| (TyVar(*root), binding.clone()))
                .collect(),
            comps: scheme
                .comp_ty_bindings
                .iter()
                .map(|(root, binding)| (CompTyVar(*root), binding.clone()))
                .collect(),
        },
    };
    ctx.name_weak(&scheme.weak);

    let quant_parts: Vec<String> = scheme
        .ty_vars
        .iter()
        .map(|(v, kind)| {
            let name = &ctx.ty_names[v];
            if kind.is_any() {
                name.clone()
            } else {
                format!("{name}:{kind}")
            }
        })
        .chain(
            scheme
                .comp_ty_vars
                .iter()
                .map(|v| ctx.comp_names[v].clone()),
        )
        .chain(scheme.grade_vars.iter().map(|v| ctx.grade_names[v].clone()))
        .chain(scheme.row_vars.iter().map(|(v, deep)| {
            let name = &ctx.row_names[v];
            if *deep {
                format!("{name}^d")
            } else {
                name.clone()
            }
        }))
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
    use super::super::kind::Kind;
    use super::super::ty::Label;
    use super::*;

    #[test]
    fn fmt_scheme_binds_cyclic_ty_roots_by_mu() {
        let root = TyVar(17);
        let scheme = Scheme {
            ty_bindings: vec![(root.0, Ty::List(Box::new(Ty::Var(root))))],
            ..Scheme::mono(Ty::List(Box::new(Ty::Var(root))))
        };
        let rendered = fmt_scheme(&scheme);
        assert_eq!(rendered, "μα. [α]");
    }

    #[test]
    fn a_node_unrolled_below_its_root_prints_as_the_root() {
        let root = TyVar(17);
        let list = Ty::List(Box::new(Ty::Var(root)));
        let deep = (0..9).fold(list.clone(), |ty, _| Ty::List(Box::new(ty)));
        let scheme = Scheme {
            ty_bindings: vec![(root.0, list)],
            ..Scheme::mono(deep)
        };
        assert_eq!(fmt_scheme(&scheme), "μα. [α]");
    }

    #[test]
    fn fmt_scheme_names_a_weak_residual_after_the_binders() {
        let (bound, weak) = (TyVar(3), TyVar(4));
        let scheme = Scheme {
            ty_vars: vec![(bound, Kind::ANY)],
            weak: WeakVars {
                tys: [(weak, Kind::ANY)].into(),
                ..WeakVars::default()
            },
            ..Scheme::mono(Ty::Record(Row::Extend(
                Label::Field("a".into()),
                Box::new(Ty::Var(bound)),
                Box::new(Row::Extend(
                    Label::Field("b".into()),
                    Box::new(Ty::Var(weak)),
                    Box::new(Row::Empty),
                )),
            )))
        };
        assert_eq!(fmt_scheme(&scheme), "∀α. [a: α, b: _β]");
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
