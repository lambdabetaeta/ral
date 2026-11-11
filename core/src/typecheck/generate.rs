//! Generate constraints and a pending tree, without observing solved types.

pub(super) mod call;
mod control;
mod data;
mod scope;

use super::env::{Inferring, TyEnv};
use super::error::{Reason, UnderAppliedBuiltin};
use super::solve::{BindHole, Solver};
use crate::ir::{
    CommandName, CommandWord, Comp, CompKind, GroupNode, Name, Pattern, Phrase, Val, is_synthetic,
};
use crate::source::{Span, Spanned, WithSpan};
use crate::ty::{CompTy, Label, Producer, Row, Scheme, Ty};
use std::collections::HashMap;
use std::sync::Arc;

/// A `Define`'s schemes before settlement: live in the unit's unifier.
pub(super) type LiveSchemes = Vec<(String, Arc<Scheme>)>;

/// A recursive group checked once: its members' schemes and its rebuilt node.
type RecGroup = (Box<[Arc<Scheme>]>, Arc<GroupNode<Inferring>>);

pub(super) struct Generator {
    pub(super) solver: Solver,
    pub(super) env: TyEnv,
    rec_groups: HashMap<*const GroupNode, RecGroup>,
    residuals: Vec<(String, Arc<Scheme>)>,
    pub(super) residual_uses: Vec<(String, Ty)>,
    pub(super) synth_counter: usize,
}

impl WithSpan for Generator {
    fn span_slot(&mut self) -> &mut Option<Span> {
        self.solver.span_slot()
    }
}

impl Generator {
    pub(super) fn new(solver: Solver, env: TyEnv, residuals: Vec<(String, Arc<Scheme>)>) -> Self {
        Self {
            solver,
            env,
            rec_groups: HashMap::new(),
            residuals,
            residual_uses: Vec::new(),
            synth_counter: 0,
        }
    }

    pub(super) fn comp(&mut self, comp: &Comp) -> (CompTy, Comp<Inferring>) {
        if let Some(span) = comp.span {
            *self.span_slot() = Some(span);
        }
        let (cty, item) = match &comp.item {
            CompKind::Protect { body, site } => self.protected_comp(body, site),
            CompKind::Return(value) => {
                let (ty, value) = self.val(value, None);
                (self.produces(ty), CompKind::Return(value))
            }
            CompKind::Lam { param, body } => {
                let param_ty = self.solver.fresh_ty();
                let (body_ty, body) = self.scope(|this| {
                    this.env
                        .bind(param.to_string(), Scheme::mono(param_ty.clone()));
                    this.comp(body)
                });
                (
                    CompTy::fun(param_ty, body_ty),
                    CompKind::Lam {
                        param: param.clone(),
                        body: Arc::new(body),
                    },
                )
            }
            CompKind::Force(value) => {
                let (ty, node) = self.val(value, None);
                (
                    self.forced(value, &ty, Reason::ForceOperand),
                    CompKind::Force(node),
                )
            }
            CompKind::Bind {
                comp: rhs,
                hole: (),
                pattern,
                rest,
            } => {
                let (rhs, hole) = self.let_(pattern, rhs);
                let (cty, rest) = self.comp(rest);
                (
                    cty,
                    CompKind::Bind {
                        comp: rhs,
                        hole,
                        pattern: Arc::clone(pattern),
                        rest: Arc::new(rest),
                    },
                )
            }
            CompKind::Seq { first, rest } => {
                let (_, first) = self.statement(first);
                let (cty, rest) = self.comp(rest);
                (
                    cty,
                    CompKind::Seq {
                        first: Arc::new(first),
                        rest: Arc::new(rest),
                    },
                )
            }
            CompKind::App {
                head,
                args,
                hole: (),
            } => self.app(head, args),
            CompKind::Exec(exec) => self.exec(comp, exec),
            &CompKind::BindingCall { hole, .. } => match hole {},
            CompKind::Pipeline { stages } => self.pipeline(stages),
            CompKind::Prim(prim) => {
                let (ty, prim) = self.prim(prim);
                (self.produces(ty), CompKind::Prim(prim))
            }
            CompKind::Rec { group, index } => self.rec(group, *index),
            CompKind::Tilde(path) => (self.produces(Ty::string()), CompKind::Tilde(path.clone())),
            CompKind::If { cond, then, else_ } => self.if_(cond, then, else_),
            CompKind::Case { scrutinee, arms } => self.case(scrutinee, arms),
            CompKind::Within {
                opts,
                handlers,
                body,
            } => self.within(opts, handlers.as_deref(), body),
            CompKind::Grant { caps, body } => self.grant(caps, body),
            CompKind::Try { body, handler } => self.try_(body, handler),
            CompKind::Guard {
                acquire,
                body,
                release,
            } => self.guard(acquire, body, release),
            CompKind::Audit { body } => self.audit(body),
            CompKind::Spawn { title, body } => self.spawn(title.as_ref(), body),
            CompKind::Gather { body } => self.gather(body),
            CompKind::Redirect { body, redirects } => self.redirect(body, redirects),
            CompKind::Capture(body) => self.capture(body),
        };
        (cty, Spanned::with_span(comp.span, item))
    }

    pub(super) fn check(&mut self, comp: &Comp, expected: &CompTy, why: Reason) -> Comp<Inferring> {
        self.with_span(comp.span, |this| {
            if let CompKind::Lam { param, body } = &comp.item {
                let param_ty = this.solver.fresh_ty();
                let result = this.solver.fresh_comp();
                let shape = CompTy::fun(param_ty.clone(), result.clone());
                this.solver.equate_comp(expected, &shape, why.clone());
                let body = this.scope(|this| {
                    this.env.bind(param.to_string(), Scheme::mono(param_ty));
                    this.check(body, &result, why)
                });
                Spanned::with_span(
                    comp.span,
                    CompKind::Lam {
                        param: param.clone(),
                        body: Arc::new(body),
                    },
                )
            } else {
                let (found, node) = this.comp(comp);
                this.solver.equate_comp(expected, &found, why);
                node
            }
        })
    }

    pub(super) fn phrases(
        &mut self,
        phrases: &[Spanned<Phrase<()>>],
    ) -> (Vec<Spanned<Phrase<LiveSchemes, Inferring>>>, Option<CompTy>) {
        let mut nodes = Vec::with_capacity(phrases.len());
        let mut tail = None;
        for phrase in phrases {
            let (node, cty) = self.with_span(phrase.span, |this| this.phrase(&phrase.item));
            nodes.push(Spanned::with_span(phrase.span, node));
            tail = cty;
        }
        (nodes, tail)
    }

    fn phrase(&mut self, phrase: &Phrase<()>) -> (Phrase<LiveSchemes, Inferring>, Option<CompTy>) {
        match phrase {
            Phrase::Define { pattern, comp, .. } => {
                let (comp, hole) = self.let_(pattern, comp);
                let schemes = pattern
                    .names()
                    .into_iter()
                    .map(|name| {
                        let scheme = self
                            .env
                            .lookup_binding(name)
                            .cloned()
                            .expect("bind_pattern just bound every collected name");
                        (name.to_string(), scheme)
                    })
                    .collect();
                (
                    Phrase::Define {
                        pattern: Arc::clone(pattern),
                        comp,
                        hole,
                        schemes,
                    },
                    None,
                )
            }
            Phrase::Run(comp) => {
                let (cty, comp) = self.statement(comp);
                (Phrase::Run(Arc::new(comp)), Some(cty))
            }
        }
    }

    fn let_(&mut self, pattern: &Pattern, rhs: &Comp) -> (Arc<Comp<Inferring>>, BindHole) {
        let forced = Self::forced_variable(rhs);
        let (node, hole, bindings) = self.with_span(rhs.span, |this| {
            this.region(|this| {
                let (cty, node) = this.comp(rhs);
                let (hole, bindings) = this.with_span(rhs.span, |this| {
                    let (ty, hole) = this.solver.binding(&cty, forced);
                    let mut bindings = Vec::new();
                    this.collect_pattern(pattern, &ty, &mut bindings);
                    (hole, bindings)
                });
                (node, hole, bindings)
            })
        });
        self.bind_pattern(bindings);
        self.note_called(pattern, rhs);
        (Arc::new(node), hole)
    }

    pub(super) fn statement(&mut self, comp: &Comp) -> (CompTy, Comp<Inferring>) {
        let (cty, node) = self.comp(comp);
        self.force_discarded_shape(comp, &cty);
        (cty, node)
    }

    fn bind_pattern(&mut self, bindings: Vec<(&Name, Ty)>) {
        let schemes: Vec<_> = bindings
            .into_iter()
            .map(|(name, ty)| (name, self.solver.generalise(&ty)))
            .collect();
        for (name, scheme) in schemes {
            self.env.bind(name.to_string(), scheme);
        }
    }

    fn collect_pattern<'p>(
        &mut self,
        pattern: &'p Pattern,
        ty: &Ty,
        bindings: &mut Vec<(&'p Name, Ty)>,
    ) {
        match pattern {
            Pattern::Wildcard => {}
            Pattern::Name(name) => bindings.push((name, ty.clone())),
            Pattern::List { elems, rest } => {
                let elem = self.solver.fresh_ty();
                self.solver
                    .equate(ty, &Ty::list(elem.clone()), Reason::ListPattern);
                for pattern in elems {
                    self.collect_pattern(pattern, &elem, bindings);
                }
                if let Some(name) = rest {
                    bindings.push((name, Ty::list(elem)));
                }
            }
            Pattern::Record(entries) => {
                let tail = self.solver.fresh_row();
                let fields: Vec<_> = entries.iter().map(|_| self.solver.fresh_ty()).collect();
                let row = entries
                    .iter()
                    .zip(&fields)
                    .rev()
                    .fold(tail, |row, (entry, ty)| {
                        Row::extend(Label::Field(entry.key.clone()), ty.clone(), row)
                    });
                self.solver
                    .equate(ty, &Ty::record(row), Reason::RecordPattern);
                for (entry, ty) in entries.iter().zip(&fields) {
                    self.collect_pattern(&entry.pattern, ty, bindings);
                }
            }
        }
    }

    fn rec(&mut self, group: &Arc<GroupNode>, index: usize) -> (CompTy, CompKind<Inferring>) {
        let key = Arc::as_ptr(group);
        if !self.rec_groups.contains_key(&key) {
            let checked = self.rec_group(group);
            self.rec_groups.insert(key, checked);
        }
        let (schemes, node) = &self.rec_groups[&key];
        let (scheme, node) = (Arc::clone(&schemes[index]), Arc::clone(node));
        let cty = self.instantiate_comp(&group.shape()[index].0, &scheme);
        (cty, CompKind::Rec { group: node, index })
    }

    /// Every projection instantiates these schemes, so the group's own
    /// variables never reach a sibling region.
    fn rec_group(&mut self, group: &GroupNode) -> RecGroup {
        let members = group.shape();
        let (types, nodes) = self.region(|this| {
            let types: Vec<_> = members.iter().map(|_| this.solver.fresh_comp()).collect();
            let nodes = this.scope(|this| {
                for ((name, _), ty) in members.iter().zip(&types) {
                    this.env
                        .bind_recursive(name.to_string(), Scheme::mono(Ty::thunk(ty.clone())));
                }
                members
                    .iter()
                    .zip(&types)
                    .map(|((name, member), ty)| {
                        let (found, node) = this.comp(member);
                        this.solver.equate_comp(&found, ty, Reason::LetRecSelf);
                        (name.clone(), Arc::new(node))
                    })
                    .collect::<Vec<_>>()
            });
            (types, nodes)
        });
        let schemes = types
            .into_iter()
            .map(|ty| Arc::new(self.solver.generalise(&Ty::thunk(ty))))
            .collect();
        (schemes, GroupNode::new(nodes.into()))
    }

    pub(super) fn produces(&mut self, ty: Ty) -> CompTy {
        CompTy::ret(self.solver.fresh_grade(), ty)
    }

    pub(super) fn forced(&mut self, value: &Val, ty: &Ty, why: Reason) -> CompTy {
        let why = match value {
            Val::Variable(name) if !is_synthetic(name) => Reason::Head {
                name: Some(name.to_string()),
            },
            _ => why,
        };
        let body = self.solver.fresh_comp();
        self.solver.equate(ty, &Ty::thunk(body.clone()), why);
        body
    }

    pub(super) fn extract_return(&mut self, cty: &CompTy) -> Ty {
        self.force_return_shape(cty, Reason::ReturnShape).ty
    }

    pub(super) fn force_return_shape(&mut self, cty: &CompTy, why: Reason) -> Producer {
        let producer = Producer {
            grade: self.solver.fresh_grade(),
            ty: self.solver.fresh_ty(),
        };
        self.solver.equate_comp(cty, &producer.clone().into(), why);
        producer
    }

    pub(super) fn force_ready_shape(&mut self, comp: &Comp, cty: &CompTy, why: Reason) -> Producer {
        let tail = Self::discard_tail(comp);
        let builtin = self.discarded_builtin_arity(tail);
        let why = match why {
            Reason::DiscardedValueShape { .. } => Reason::DiscardedValueShape { builtin },
            Reason::PipelineStageShape { .. } => Reason::PipelineStageShape { builtin },
            other => other,
        };
        self.with_span(tail.span, |this| this.force_return_shape(cty, why))
    }

    pub(super) fn force_discarded_shape(&mut self, comp: &Comp, cty: &CompTy) {
        self.force_ready_shape(comp, cty, Reason::DiscardedValueShape { builtin: None });
    }

    fn discard_tail(mut comp: &Comp) -> &Comp {
        while let CompKind::Bind { rest, .. } | CompKind::Seq { rest, .. } = &comp.item {
            comp = rest;
        }
        comp
    }

    fn discarded_builtin_arity(&self, comp: &Comp) -> Option<UnderAppliedBuiltin> {
        let CompKind::Exec(exec) = &comp.item else {
            return None;
        };
        let CommandWord::Name(CommandName::Bare(name)) = &exec.head else {
            return None;
        };
        if self.env.lookup_binding(name).is_some() {
            return None;
        }
        let entry = self.env.builtins.value(name)?;
        Some(UnderAppliedBuiltin {
            name: name.to_string(),
            expected: entry.fixed_arity(),
            got: exec.argv.arity().unwrap_or(0),
        })
    }

    pub(super) fn result_span(comp: &Comp) -> Option<Span> {
        let comp = Self::discard_tail(comp);
        match &comp.item {
            CompKind::Lam { body, .. } => Self::result_span(body),
            _ => comp.span,
        }
    }

    fn note_called(&mut self, pattern: &Pattern, rhs: &Comp) {
        if let Pattern::Name(name) = pattern
            && !is_synthetic(name)
            && let Some(callee) = Self::called_head(rhs)
        {
            self.env.note_called(name.to_string(), callee);
        }
    }

    /// The variable `comp` forces, when it is `!$x` (or a bare call of `x`).
    pub(super) fn forced_variable(comp: &Comp) -> Option<&str> {
        match &comp.item {
            CompKind::Force(Val::Variable(name)) if !is_synthetic(name) => Some(name),
            _ => None,
        }
    }

    fn called_head(rhs: &Comp) -> Option<String> {
        match &Self::discard_tail(rhs).item {
            CompKind::Pipeline { stages } => {
                stages.last().and_then(|stage| Self::called_head(stage))
            }
            CompKind::Force(Val::Variable(name)) if !is_synthetic(name) => Some(name.to_string()),
            CompKind::App { head, .. } => Self::called_head(head),
            CompKind::Exec(exec) => Some(exec.head.name().written().into_owned()),
            _ => None,
        }
    }

    pub(super) fn region<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let at = *self.span_slot();
        self.solver.enter();
        let result = f(self);
        // Defaults belong to the region, not its last visited child.
        self.with_span(at, |this| this.solver.exit());
        result
    }

    pub(super) fn scope<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = *self.span_slot();
        self.env.push();
        let result = f(self);
        self.env.pop();
        *self.span_slot() = saved;
        result
    }

    fn note_residual_use(&mut self, name: &str, scheme: &Arc<Scheme>, ty: &Ty) -> bool {
        let residual = self
            .residuals
            .iter()
            .any(|(n, stored)| n == name && Arc::ptr_eq(stored, scheme));
        if residual && !self.residual_uses.iter().any(|(n, _)| n == name) {
            self.residual_uses.push((name.to_owned(), ty.clone()));
        }
        residual
    }
}

#[cfg(test)]
mod tests {
    use crate::syntax::parser::parse;
    use crate::{SessionSchemes, elaborator::elaborate, typecheck};

    pub(super) fn error_codes(src: &str) -> Vec<&'static str> {
        error_codes_seeded(src, Vec::new())
    }

    /// `src` checked against a session holding `bindings`, which the
    /// elaborator is not told of, so a head naming one reaches the checker as
    /// an `Exec`.
    pub(super) fn error_codes_seeded(
        src: &str,
        bindings: Vec<(String, Option<crate::ty::Closed>)>,
    ) -> Vec<&'static str> {
        let ast = parse(src).unwrap_or_else(|e| panic!("parse error in {src:?}: {e:?}"));
        let comp = elaborate(&ast, [], None)
            .unwrap_or_else(|e| panic!("elaborate error in {src:?}: {e:?}"));
        let schemes = SessionSchemes {
            bindings,
            ..crate::test_helper::core_schemes()
        };
        typecheck(&comp, schemes, None)
            .err()
            .unwrap_or_default()
            .iter()
            .map(|e| e.kind.code())
            .collect()
    }

    use super::*;
    use crate::ir::FieldPattern;
    use crate::path::TildePath;
    use crate::source::FileId;
    use crate::ty::Grade;
    use crate::typecheck::Unifier;
    use crate::typecheck::solve::{Mode, Solution};

    pub(super) fn solved(solver: Solver) -> Solution {
        solver
            .settle()
            .close()
            .unwrap_or_else(|errors| panic!("{errors:?}"))
    }

    pub(super) fn assert_solved_type(store: &Unifier, actual: &Ty, expected: &Ty) {
        let shapes = |ty| {
            crate::ty::Site::snapshot(store, ty, crate::ty::UnitId::fresh())
                .nodes
                .iter()
                .map(|node| node.shape.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(shapes(actual), shapes(expected));
    }

    pub(super) fn assert_solved_comp(store: &Unifier, actual: &CompTy, expected: &CompTy) {
        assert_solved_type(
            store,
            &Ty::thunk(actual.clone()),
            &Ty::thunk(expected.clone()),
        );
    }

    fn generator() -> Generator {
        Generator::new(Solver::new(), TyEnv::new(), Vec::new())
    }

    fn at(start: u32) -> Span {
        Span::new(FileId::DUMMY, start, start + 1)
    }

    fn tilde(start: u32) -> Comp {
        Spanned::new(at(start), CompKind::Tilde(TildePath { suffix: None }))
    }

    fn binding(generator: &mut Generator, name: &str) -> Ty {
        let scheme = Arc::clone(generator.env.lookup_binding(name).expect("bound name"));
        generator.solver.instantiate(&scheme)
    }

    #[test]
    fn define_rhs_carries_its_binding_hole_and_exports_the_environment_token() {
        let mut generator = generator();
        let source = Spanned::new(
            at(0),
            Phrase::Define {
                pattern: Arc::new(Pattern::Name("home".into())),
                comp: Arc::new(tilde(1)),
                hole: (),
                schemes: (),
            },
        );
        let (phrases, tail) = generator.phrases(&[source]);
        assert!(tail.is_none());
        assert_eq!(phrases[0].span, Some(at(0)));
        let Phrase::Define {
            comp,
            hole,
            schemes,
            ..
        } = &phrases[0].item
        else {
            panic!("definition preserved")
        };
        assert!(Arc::ptr_eq(
            &schemes[0].1,
            generator.env.lookup_binding("home").unwrap()
        ));
        assert!(matches!(comp.item, CompKind::Tilde(_)));
        assert_eq!(comp.span, Some(at(1)));
        let hole = *hole;
        let home = binding(&mut generator, "home");
        let solution = solved(generator.solver);
        assert_eq!(solution.mode(hole), Mode::Run);
        assert_eq!(solution.resolve_ty(&home), Ty::string());
    }

    #[test]
    fn check_lambda_equates_its_shape_at_the_lambda_before_checking_the_body() {
        let mut generator = generator();
        let source = Spanned::new(
            at(1),
            CompKind::Lam {
                param: "arg".into(),
                body: Arc::new(tilde(2)),
            },
        );
        let node = generator.check(&source, &CompTy::pure(Ty::string()), Reason::Argument);
        assert_eq!(node.span, Some(at(1)));
        assert!(generator.env.lookup_binding("arg").is_none());
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].pos, Some(at(1)));
        assert!(matches!(errors[0].reason, Some(Reason::Argument)));
    }

    #[test]
    fn pattern_binders_generalise_only_after_their_region_closes() {
        let mut generator = generator();
        let pattern = Pattern::Record(vec![FieldPattern {
            key: "items".into(),
            pattern: Pattern::List {
                elems: vec![Pattern::Name("first".into()), Pattern::Wildcard],
                rest: Some("rest".into()),
            },
        }]);
        let bindings = generator.region(|this| {
            let ty = this.solver.fresh_ty();
            let mut bindings = Vec::new();
            this.collect_pattern(&pattern, &ty, &mut bindings);
            bindings
        });
        generator.bind_pattern(bindings);
        let first = binding(&mut generator, "first");
        let rest = binding(&mut generator, "rest");
        assert!(
            generator
                .solver
                .equate(&first, &Ty::int(), Reason::ListPattern)
        );
        assert!(
            generator
                .solver
                .equate(&rest, &Ty::list(Ty::bool()), Reason::ListPattern)
        );
        solved(generator.solver);
    }

    #[test]
    fn pattern_reachable_from_an_older_unknown_stays_monomorphic() {
        let mut generator = generator();
        let older = generator.solver.fresh_ty();
        let pattern = Pattern::List {
            elems: vec![Pattern::Name("item".into())],
            rest: None,
        };
        let bindings = generator.region(|this| {
            let mut bindings = Vec::new();
            this.collect_pattern(&pattern, &older, &mut bindings);
            bindings
        });
        generator.bind_pattern(bindings);
        let first = binding(&mut generator, "item");
        let second = binding(&mut generator, "item");
        assert!(
            generator
                .solver
                .equate(&first, &Ty::int(), Reason::ListPattern)
        );
        assert!(
            !generator
                .solver
                .equate(&second, &Ty::bool(), Reason::ListPattern)
        );
        assert_eq!(generator.solver.refused().1.len(), 1);
    }

    #[test]
    fn rec_sibling_lets_share_one_group_and_restore_shadowed_bindings() {
        let mut generator = generator();
        let original = Arc::new(Scheme::mono(Ty::string()));
        generator.env.bind("self".into(), Arc::clone(&original));
        let group = GroupNode::new(
            vec![(
                "self".into(),
                Arc::new(Spanned::new(
                    at(1),
                    CompKind::Lam {
                        param: "x".into(),
                        body: Arc::new(tilde(2)),
                    },
                )),
            )]
            .into(),
        );
        let source = Spanned::new(at(0), CompKind::Rec { group, index: 0 });
        let (first, _) = generator.let_(&Pattern::Name("first".into()), &source);
        let (second, _) = generator.let_(&Pattern::Name("second".into()), &source);
        let group_of = |node: &Comp<Inferring>| {
            let CompKind::Rec { group, .. } = &node.item else {
                panic!("rec projection")
            };
            Arc::clone(group)
        };
        assert!(Arc::ptr_eq(&group_of(&first), &group_of(&second)));
        assert!(Arc::ptr_eq(
            generator.env.lookup_binding("self").unwrap(),
            &original
        ));
        for name in ["first", "second"] {
            for param in [Ty::int(), Ty::bool()] {
                let instance = binding(&mut generator, name);
                let expected = Ty::thunk(CompTy::arrows([param], CompTy::pure(Ty::string())));
                assert!(
                    generator
                        .solver
                        .equate(&instance, &expected, Reason::LetRecSelf)
                );
            }
        }
        solved(generator.solver);
    }

    #[test]
    fn rec_sibling_constraining_its_projection_leaves_the_next_polymorphic() {
        let mut generator = generator();
        let group = GroupNode::new(
            vec![(
                "self".into(),
                Arc::new(Spanned::new(
                    at(1),
                    CompKind::Lam {
                        param: "x".into(),
                        body: Arc::new(tilde(2)),
                    },
                )),
            )]
            .into(),
        );
        let projection = Arc::new(Spanned::new(at(0), CompKind::Rec { group, index: 0 }));
        let applied = Spanned::new(
            at(3),
            CompKind::App {
                head: Arc::clone(&projection),
                args: [Spanned::new(at(4), Val::Int(1))].into(),
                hole: (),
            },
        );
        generator.let_(&Pattern::Name("applied".into()), &applied);
        generator.let_(&Pattern::Name("other".into()), &projection);
        let other = binding(&mut generator, "other");
        let expected = Ty::thunk(CompTy::arrows([Ty::bool()], CompTy::pure(Ty::string())));
        assert!(
            generator
                .solver
                .equate(&other, &expected, Reason::LetRecSelf)
        );
        solved(generator.solver);
    }

    #[test]
    fn forced_and_producer_shapes_are_equations_with_fresh_parts() {
        let mut generator = generator();
        let ty = generator.solver.fresh_ty();
        let body = generator.forced(&Val::Variable("f".into()), &ty, Reason::ForceOperand);
        generator
            .solver
            .equate_comp(&body, &CompTy::command(), Reason::Argument);
        let producer = generator.force_return_shape(&body, Reason::ReturnShape);
        let solution = solved(generator.solver);
        assert_solved_type(&solution.scratch(), &ty, &Ty::thunk(CompTy::command()));
        assert_eq!(solution.resolve_ty(&producer.ty), Ty::unit());
        assert_solved_comp(
            &solution.scratch(),
            &producer.into(),
            &CompTy::ret(Grade::Output, Ty::unit()),
        );
    }

    #[test]
    fn forced_named_non_thunk_is_reported_at_the_written_use() {
        let mut generator = generator();
        generator.with_span(Some(at(3)), |this| {
            this.forced(
                &Val::Variable("f".into()),
                &Ty::string(),
                Reason::ForceOperand,
            );
        });
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].pos, Some(at(3)));
        assert_eq!(errors[0].kind.code(), "T0072");
    }

    #[test]
    fn scope_preserves_span_scheme_identity_and_name_supply() {
        let stored = Arc::new(Scheme::mono(Ty::int()));
        let mut generator = Generator::new(
            Solver::new(),
            TyEnv::new(),
            vec![("imported".into(), Arc::clone(&stored))],
        );
        generator.env.bind("imported".into(), Arc::clone(&stored));
        *generator.span_slot() = Some(at(1));
        let first = crate::typecheck::readback::fresh_name(&mut generator.synth_counter, "eta");
        generator.scope(|this| {
            this.env.bind("imported".into(), Scheme::mono(Ty::string()));
            *this.span_slot() = Some(at(2));
            assert_ne!(
                crate::typecheck::readback::fresh_name(&mut this.synth_counter, "eta"),
                first
            );
        });
        assert_eq!(*generator.span_slot(), Some(at(1)));
        assert!(Arc::ptr_eq(
            generator.env.lookup_binding("imported").unwrap(),
            &stored
        ));
        let ty = generator.solver.instantiate(&stored);
        assert!(generator.note_residual_use("imported", &stored, &ty));
        assert!(generator.note_residual_use("imported", &stored, &ty));
        assert_eq!(generator.residual_uses.len(), 1);
        assert_eq!(generator.synth_counter, 2);
    }
}
