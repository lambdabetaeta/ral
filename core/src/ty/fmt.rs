//! Rendering types as text, for error messages and `:type` output.
//!
//! Pure functions over the type algebra; nothing here consults the unifier.
//! A diagnostic that prints two types must give a shared variable the same
//! name in both, so callers first build one [`FmtCtx`] over everything they
//! are about to render: it mints Greek letters in first-appearance order.

use super::scheme::{Role, Scheme};
use super::{
    CompTy, CompTyNode, CompTyVar, Grade, GradeVar, Head, Kind, Row, RowNode, RowVar, Term, Ty,
    TyNode, TyVar, Var,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

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
    ty_kinds: HashMap<TyVar, Kind>,
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
    tys: HashSet<(Ty, Ty)>,
    comps: HashSet<(CompTy, CompTy)>,
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
        if !seen.tys.insert((a.clone(), b.clone())) {
            return true;
        }
        match (a.node(), b.node()) {
            (TyNode::Var(v), _) if let Some(a) = self.ty_binding(*v) => self.ty_eq(a, b, seen),
            (_, TyNode::Var(v)) if let Some(b) = self.ty_binding(*v) => self.ty_eq(a, b, seen),
            (TyNode::List(x), TyNode::List(y)) | (TyNode::Map(x), TyNode::Map(y)) => {
                self.ty_eq(x, y, seen)
            }
            (TyNode::Handle(g, x), TyNode::Handle(h, y)) => g == h && self.ty_eq(x, y, seen),
            (TyNode::Record(x), TyNode::Record(y)) | (TyNode::Variant(x), TyNode::Variant(y)) => {
                self.row_eq(x, y, seen)
            }
            (TyNode::Thunk(x), TyNode::Thunk(y)) => self.comp_eq(x, y, seen),
            _ => a == b,
        }
    }

    fn comp_eq(&self, a: &CompTy, b: &CompTy, seen: &mut Assumed) -> bool {
        if !seen.comps.insert((a.clone(), b.clone())) {
            return true;
        }
        match (a.node(), b.node()) {
            (CompTyNode::Var(v), _) if let Some(a) = self.comp_binding(*v) => {
                self.comp_eq(a, b, seen)
            }
            (_, CompTyNode::Var(v)) if let Some(b) = self.comp_binding(*v) => {
                self.comp_eq(a, b, seen)
            }
            (CompTyNode::Return(g, x), CompTyNode::Return(h, y)) => {
                g == h && self.ty_eq(x, y, seen)
            }
            (CompTyNode::Fun(p, x), CompTyNode::Fun(q, y)) => {
                self.ty_eq(p, q, seen) && self.comp_eq(x, y, seen)
            }
            _ => a == b,
        }
    }

    fn row_eq(&self, a: &Row, b: &Row, seen: &mut Assumed) -> bool {
        match (a.node(), b.node()) {
            (RowNode::Extend(l, f, x), RowNode::Extend(m, g, y)) => {
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
    fn name_weak(&mut self, weak: impl Iterator<Item = Var>) {
        for var in weak {
            match var {
                Var::Ty(v) => {
                    let name = format!("_{}", pick(TY_LETTERS, self.ty_names.len()));
                    self.ty_names.insert(v, name);
                }
                Var::Comp(v) => {
                    let name = format!("_{}", pick(COMP_LETTERS, self.comp_names.len()));
                    self.comp_names.insert(v, name);
                }
                Var::Row(v) => {
                    let name = format!("_{}", pick(ROW_LETTERS, self.row_names.len()));
                    self.row_names.insert(v, name);
                }
                Var::Grade(_) => {}
            }
        }
    }

    /// Name every unification variable in `types`, in first-appearance order.
    /// Pass every type that will be rendered side by side, so a variable they
    /// share gets one name.
    pub(crate) fn for_value_types(types: &[&Ty]) -> Self {
        let mut ctx = Self::default();
        for t in types {
            ctx.absorb(Term::Ty(t));
        }
        ctx
    }

    /// [`Self::for_value_types`], for computations.
    pub(crate) fn for_comp_types(types: &[&CompTy]) -> Self {
        let mut ctx = Self::default();
        for t in types {
            ctx.absorb(Term::Comp(t));
        }
        ctx
    }

    fn excludes_unit(&self, ty: &Ty) -> bool {
        match ty.node() {
            TyNode::Unit => false,
            TyNode::Var(v) => match self.cycles.ty_binding(*v) {
                Some(binding) => !matches!(binding.node(), TyNode::Unit | TyNode::Var(_)),
                None => self.ty_kinds.get(v).is_some_and(|k| !k.admits(Head::Unit)),
            },
            _ => true,
        }
    }

    fn visible_grade(&self, term: Term<'_>) -> Option<GradeVar> {
        let (grade, ty) = match term {
            Term::Ty(ty) => match ty.node() {
                TyNode::Handle(grade, ty) => (grade, ty),
                _ => return None,
            },
            Term::Comp(cty) => match cty.node() {
                CompTyNode::Return(grade, ty) => (grade, ty),
                _ => return None,
            },
            Term::Row(_) => return None,
        };
        match grade {
            Grade::Var(v) if !self.excludes_unit(ty) => Some(*v),
            _ => None,
        }
    }

    /// Follow each cyclic root once when finding printed grades.
    fn visible_grades(&self, term: Term<'_>, seen: &mut Vec<Open>, grades: &mut HashSet<GradeVar>) {
        let rooted = match term {
            Term::Ty(ty) => self
                .cycles
                .ty_root(ty)
                .map(|(v, binding)| (Open::Ty(*v), Term::Ty(binding))),
            Term::Comp(cty) => self
                .cycles
                .comp_root(cty)
                .map(|(v, binding)| (Open::Comp(*v), Term::Comp(binding))),
            Term::Row(_) => None,
        };
        let term = if let Some((root, binding)) = rooted {
            if seen.contains(&root) {
                return;
            }
            seen.push(root);
            binding
        } else {
            term
        };
        if let Some(v) = self.visible_grade(term) {
            grades.insert(v);
        }
        term.children()
            .for_each(|child| self.visible_grades(child, seen, grades));
    }

    /// Name every variable under `term` not yet named, pre-order.
    fn absorb(&mut self, term: Term<'_>) {
        if let Some(v) = self.visible_grade(term) {
            let idx = self.grade_names.len();
            self.grade_names
                .entry(v)
                .or_insert_with(|| pick(GRADE_LETTERS, idx));
        }
        match term {
            Term::Ty(ty) => {
                if let TyNode::Var(v) = ty.node() {
                    let idx = self.ty_names.len();
                    self.ty_names
                        .entry(*v)
                        .or_insert_with(|| pick(TY_LETTERS, idx));
                }
            }
            Term::Comp(cty) => {
                if let CompTyNode::Var(v) = cty.node() {
                    let idx = self.comp_names.len();
                    self.comp_names
                        .entry(*v)
                        .or_insert_with(|| pick(COMP_LETTERS, idx));
                }
            }
            Term::Row(row) => {
                if let RowNode::Var(v) = row.node() {
                    let idx = self.row_names.len();
                    self.row_names
                        .entry(*v)
                        .or_insert_with(|| pick(ROW_LETTERS, idx));
                }
            }
        }
        term.children().for_each(|child| self.absorb(child));
    }
}

impl fmt::Display for Ty {
    /// Variables print as placeholders; a diagnostic naming several types
    /// goes through [`FmtCtx::for_value_types`] instead.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&fmt_ty_ctx(self, &FmtCtx::default()))
    }
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
    match ty.node() {
        TyNode::Unit => "Unit".into(),
        TyNode::Bytes => "Bytes".into(),
        TyNode::Bool => "Bool".into(),
        TyNode::Int => "Integer".into(),
        TyNode::Float => "Float".into(),
        TyNode::String => "String".into(),
        TyNode::Handle(g, a) => {
            let producer = fmt_producer(*g, a, ctx, open);
            if producer.contains(' ') {
                format!("Handle ({producer})")
            } else {
                format!("Handle {producer}")
            }
        }
        TyNode::Var(v) => ctx.ty_name(*v),
        TyNode::List(a) => format!("[{}]", fmt_ty_in(a, ctx, open)),
        TyNode::Map(a) => format!("Map {}", fmt_ty_in(a, ctx, open)),
        TyNode::Record(r) => format!("[{}]", fmt_row_in(r, ctx, open)),
        TyNode::Variant(r) => format!("[{}]", fmt_variant_row_in(r, ctx, open)),
        TyNode::Thunk(b) => format!("{{{}}}", fmt_comp_ty_in(b, ctx, open)),
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
    let parts: Vec<String> = row
        .fields()
        .map(|(l, ty)| format!("{l}: {}", fmt_ty_in(ty, ctx, open)))
        .collect();
    let tail = match row.tail().node() {
        RowNode::Var(v) => Some(match ctx.row_name(*v) {
            Some(name) => format!("{tail_sigil}...{name}"),
            None => format!("{tail_sigil}..."),
        }),
        _ => None,
    };
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
    match cty.node() {
        CompTyNode::Var(v) => ctx.comp_name(*v),
        CompTyNode::Fun(a, b) => format!(
            "{} → {}",
            fmt_ty_in(a, ctx, open),
            fmt_comp_ty_in(b, ctx, open)
        ),
        CompTyNode::Return(g, a) => fmt_producer(*g, a, ctx, open),
    }
}

/// An `F` under its grade: what a `Return` prints as, and what a handle holds.
fn fmt_producer(grade: Grade, a: &Ty, ctx: &FmtCtx, open: &mut Vec<Open>) -> String {
    match grade {
        Grade::Output => "Command".into(),
        Grade::Value => format!("Returns {}", fmt_ty_in(a, ctx, open)),
        Grade::Var(_) if ctx.excludes_unit(a) => format!("Returns {}", fmt_ty_in(a, ctx, open)),
        Grade::Var(v) => format!("Command[{}] {}", ctx.grade_name(v), fmt_ty_in(a, ctx, open)),
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

/// A scheme with its ∀ prefix, naming variables by their position in the
/// scheme's quantifier lists.
///
/// The outer `Thunk` is stripped, so a command reads `Command`, not `{Command}`.
impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&render_scheme(self))
    }
}

fn render_scheme(scheme: &Scheme) -> String {
    let forall_tys = foralls(&scheme.tys);
    let forall_comps: Vec<CompTyVar> = foralls(&scheme.comps)
        .into_iter()
        .map(|(v, ())| v)
        .collect();
    let forall_rows = foralls(&scheme.rows);
    let cycles = Cycles {
        tys: mus(&scheme.tys),
        comps: mus(&scheme.comps),
    };

    // Roots of cyclic bindings are named after the plain vars; they bind by `μ`, not `∀`.
    let ty_order: Vec<TyVar> = forall_tys
        .iter()
        .map(|&(v, _)| v)
        .chain(cycles.tys.iter().map(|&(v, _)| v))
        .collect();
    let comp_order: Vec<CompTyVar> = forall_comps
        .iter()
        .copied()
        .chain(cycles.comps.iter().map(|&(v, _)| v))
        .collect();
    let row_order: Vec<RowVar> = forall_rows.iter().map(|&(v, _)| v).collect();
    let mut ctx = FmtCtx {
        ty_names: names_in_order(&ty_order, TY_LETTERS),
        comp_names: names_in_order(&comp_order, COMP_LETTERS),
        row_names: names_in_order(&row_order, ROW_LETTERS),
        ty_kinds: forall_tys.iter().copied().collect(),
        grade_names: HashMap::new(),
        cycles,
    };
    let mut visible = HashSet::new();
    ctx.visible_grades(Term::Ty(&scheme.ty), &mut Vec::new(), &mut visible);
    let grade_order: Vec<GradeVar> = scheme
        .grades
        .iter()
        .copied()
        .filter(|v| visible.contains(v))
        .collect();
    ctx.grade_names = names_in_order(&grade_order, GRADE_LETTERS);
    ctx.name_weak(scheme.weak());

    let quant_parts: Vec<String> = forall_tys
        .iter()
        .map(|(v, kind)| {
            let name = &ctx.ty_names[v];
            if kind.is_any() {
                name.clone()
            } else {
                format!("{name}:{kind}")
            }
        })
        .chain(forall_comps.iter().map(|v| ctx.comp_names[v].clone()))
        .chain(grade_order.iter().map(|v| ctx.grade_names[v].clone()))
        .chain(forall_rows.iter().map(|(v, deep)| {
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

    let body = match scheme.ty.node() {
        TyNode::Thunk(cty) => fmt_comp_ty_ctx(cty, &ctx),
        _ => fmt_ty_ctx(&scheme.ty, &ctx),
    };

    format!("{prefix}{body}")
}

/// The `∀`-bound variables of `vars`, each with its meta.
fn foralls<V: Copy, M: Copy, B>(vars: &BTreeMap<V, Role<M, B>>) -> Vec<(V, M)> {
    vars.iter()
        .filter_map(|(&v, role)| Some((v, role.forall()?)))
        .collect()
}

/// The μ-roots of `vars`, each with its binding.
fn mus<V: Copy, M: Copy, B: Clone>(vars: &BTreeMap<V, Role<M, B>>) -> Vec<(V, B)> {
    vars.iter()
        .filter_map(|(&v, role)| Some((v, role.mu()?.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::Label;
    use super::super::kind::Kind;
    use super::*;

    #[test]
    fn fmt_scheme_binds_cyclic_comp_roots_by_mu() {
        let root = CompTyVar(29);
        let binding = CompTy::pure(Ty::list(Ty::thunk(CompTy::var(root))));
        let scheme = Scheme {
            comps: [(root, Role::Mu(binding.clone()))].into(),
            ..Scheme::mono(Ty::thunk(binding))
        };
        assert_eq!(scheme.to_string(), "μϕ. Returns [{ϕ}]");
    }

    #[test]
    fn a_node_unrolled_below_its_root_prints_as_the_root() {
        let root = TyVar(17);
        let list = Ty::list(Ty::var(root));
        let deep = (0..9).fold(list.clone(), |ty, _| Ty::list(ty));
        let scheme = Scheme {
            tys: [(root, Role::Mu(list))].into(),
            ..Scheme::mono(deep)
        };
        assert_eq!(scheme.to_string(), "μα. [α]");
    }

    #[test]
    fn fmt_scheme_names_a_weak_residual_after_the_binders() {
        let (bound, weak) = (TyVar(3), TyVar(4));
        let scheme = Scheme {
            tys: [
                (bound, Role::Forall(Kind::ANY)),
                (weak, Role::Weak(Kind::ANY)),
            ]
            .into(),
            ..Scheme::mono(Ty::record(Row::extend(
                Label::Field("a".into()),
                Ty::var(bound),
                Row::extend(Label::Field("b".into()), Ty::var(weak), Row::empty()),
            )))
        };
        assert_eq!(scheme.to_string(), "∀α. [a: α, b: _β]");
    }
}
