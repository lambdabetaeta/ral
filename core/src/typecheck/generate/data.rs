//! Values and primitives.

use super::Generator;
use crate::ir::{
    ArithOp, BinaryOp, FieldsNode, Key, MapEntryV, Name, Prim, Val, ValListElem, is_synthetic,
};
use crate::source::{Span, Spanned, WithSpan};
use crate::ty::{CompTy, Kind, Label, Row, Site, Ty};
use crate::typecheck::builtins::{Decl, LANGUAGE_CONSTANTS};
use crate::typecheck::env::{Inferring, Membrane};
use crate::typecheck::error::{Reason, TypeErrorKind, UnitCall};
use crate::typecheck::readback;
use std::sync::Arc;

fn spell_literal(v: &Val) -> Option<String> {
    match v {
        Val::String(s) => Some(format!("'{s}'")),
        Val::Int(n) => Some(n.to_string()),
        Val::Float(f) => Some(f.to_string()),
        Val::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn written(v: &Val) -> Option<&Name> {
    match v {
        Val::Variable(x) if !is_synthetic(x) => Some(x),
        _ => None,
    }
}

fn spell_key(key: &Key) -> String {
    match key {
        Key::Label(label) => label.to_string(),
        Key::Position(n) => n.to_string(),
        Key::Data(v) => written(v).map_or_else(|| "…".into(), |x| format!("${x}")),
    }
}

impl Generator {
    fn diagnose_if_duplicate<'a>(
        &mut self,
        key: &str,
        existing: impl Iterator<Item = &'a str>,
    ) -> bool {
        let duplicate = existing.into_iter().any(|seen| seen == key);
        if duplicate {
            self.solver.diagnose(TypeErrorKind::DuplicateField {
                label: key.to_string(),
            });
        }
        duplicate
    }

    // Last-declared fields are innermost.
    fn fields_row(fields: Vec<(String, Ty)>, tail: Row) -> Row {
        fields.into_iter().rev().fold(tail, |rest, (key, ty)| {
            Row::Extend(Label::Field(key), Box::new(ty), Box::new(rest))
        })
    }

    fn record_field(
        &mut self,
        key: &str,
        value: &Spanned<Val>,
        fields: &mut Vec<(String, Ty)>,
    ) -> Spanned<Val<Inferring>> {
        let (duplicate, ty, node) = self.with_span(value.span, |this| {
            let duplicate = this.diagnose_if_duplicate(key, fields.iter().map(|(k, _)| k.as_str()));
            let (ty, node) = this.val(&value.item, value.span);
            (duplicate, ty, node)
        });
        if !duplicate {
            fields.push((key.to_string(), ty));
        }
        Spanned::with_span(value.span, node)
    }

    fn record_val(&mut self, entries: &[(Name, Spanned<Val>)]) -> (Ty, Arc<FieldsNode<Inferring>>) {
        let mut fields: Vec<(String, Ty)> = Vec::new();
        let nodes = entries
            .iter()
            .map(|(key, value)| (key.clone(), self.record_field(key, value, &mut fields)))
            .collect();
        (
            Ty::Record(Self::fields_row(fields, Row::Empty)),
            FieldsNode::new(nodes),
        )
    }

    fn update(
        &mut self,
        base: &Spanned<Val>,
        entries: &[(Name, Spanned<Val>)],
    ) -> (Ty, Prim<Inferring>) {
        let (base_ty, base_node) =
            self.with_span(base.span, |this| this.val(&base.item, base.span));
        let mut fields = Vec::new();
        let nodes = entries
            .iter()
            .map(|(key, value)| (key.clone(), self.record_field(key, value, &mut fields)))
            .collect();
        let tail = self.solver.fresh_row();
        let probe = fields.iter().rev().fold(tail.clone(), |rest, (key, _)| {
            let old = self.solver.fresh_ty();
            Row::Extend(Label::Field(key.clone()), Box::new(old), Box::new(rest))
        });
        let reason = Reason::RecordUpdate {
            base: written(&base.item).map(ToString::to_string),
        };
        self.with_span(base.span, |this| {
            this.solver.equate(&base_ty, &Ty::Record(probe), reason);
        });
        (
            Ty::Record(Self::fields_row(fields, tail)),
            Prim::Update {
                base: Spanned::with_span(base.span, base_node),
                fields: nodes,
            },
        )
    }

    fn map_val(&mut self, entries: &[(Name, Spanned<Val>)]) -> (Ty, Arc<FieldsNode<Inferring>>) {
        let elem = self.solver.fresh_ty();
        let mut seen: Vec<Name> = Vec::new();
        let mut nodes = Vec::new();
        for (key, value) in entries {
            let (duplicate, node) = self.with_span(value.span, |this| {
                let duplicate = this.diagnose_if_duplicate(key, seen.iter().map(AsRef::as_ref));
                let (value_ty, node) = this.val(&value.item, value.span);
                this.solver.equate(&value_ty, &elem, Reason::MapElem);
                (duplicate, node)
            });
            if !duplicate {
                seen.push(key.clone());
            }
            nodes.push((key.clone(), Spanned::with_span(value.span, node)));
        }
        (Ty::map(elem), FieldsNode::new(nodes.into()))
    }

    fn map(&mut self, entries: &[MapEntryV]) -> (Ty, Prim<Inferring>) {
        let elem = self.solver.fresh_ty();
        let mut seen: Vec<String> = Vec::new();
        let mut nodes = Vec::new();
        for entry in entries {
            match entry {
                MapEntryV::Entry(key, value) => {
                    let (key_ty, key_node) =
                        self.with_span(key.span, |this| this.val(&key.item, key.span));
                    self.with_span(key.span, |this| {
                        this.solver.equate(&key_ty, &Ty::String, Reason::MapKey);
                        if let Val::String(label) = &key.item {
                            this.diagnose_if_duplicate(
                                label.as_str(),
                                seen.iter().map(String::as_str),
                            );
                            seen.push(label.as_str().to_string());
                        }
                    });
                    let (value_ty, value_node) =
                        self.with_span(value.span, |this| this.val(&value.item, value.span));
                    self.with_span(value.span, |this| {
                        this.solver.equate(&value_ty, &elem, Reason::MapElem);
                    });
                    nodes.push(MapEntryV::Entry(
                        Spanned::with_span(key.span, key_node),
                        Spanned::with_span(value.span, value_node),
                    ));
                }
                MapEntryV::Spread(value) => {
                    let (spread_ty, value_node) =
                        self.with_span(value.span, |this| this.val(&value.item, value.span));
                    self.with_span(value.span, |this| {
                        this.solver
                            .equate(&spread_ty, &Ty::map(elem.clone()), Reason::MapSpread);
                    });
                    nodes.push(MapEntryV::Spread(Spanned::with_span(
                        value.span, value_node,
                    )));
                }
            }
        }
        (Ty::map(elem), Prim::Map(nodes.into()))
    }

    fn list_entry(
        &mut self,
        value: &Spanned<Val>,
        spread: bool,
        elem: &Ty,
    ) -> Spanned<Val<Inferring>> {
        self.with_span(value.span, |this| {
            let (ty, node) = this.val(&value.item, value.span);
            let entry_ty = if spread {
                let inner = this.solver.fresh_ty();
                this.solver
                    .equate(&ty, &Ty::list(inner.clone()), Reason::ListSpread);
                inner
            } else {
                ty
            };
            this.solver.equate(&entry_ty, elem, Reason::ListElem);
            Spanned::with_span(value.span, node)
        })
    }

    fn list(&mut self, elems: &[ValListElem]) -> (Ty, Prim<Inferring>) {
        let elem = self.solver.fresh_ty();
        let nodes = elems
            .iter()
            .map(|entry| match entry {
                ValListElem::Single(v) => ValListElem::Single(self.list_entry(v, false, &elem)),
                ValListElem::Spread(v) => ValListElem::Spread(self.list_entry(v, true, &elem)),
            })
            .collect();
        (Ty::list(elem), Prim::List(nodes))
    }

    fn diagnose_unbound(&mut self, name: &str) {
        let candidates = self
            .env
            .binding_names()
            .chain(self.env.builtins.value_names())
            .chain(LANGUAGE_CONSTANTS.iter().map(|&(name, _)| name));
        let suggestions = crate::text::near_names(name, candidates, 3)
            .into_iter()
            .map(str::to_string)
            .collect();
        let boundary = crate::typecheck::explain::hyphen_boundary(name, |p| {
            self.env.lookup_binding(p).is_some()
                || self.env.builtins.value_names().any(|n| n == p)
                || LANGUAGE_CONSTANTS.iter().any(|&(c, _)| c == p)
        })
        .map(str::to_string);
        self.solver.diagnose(TypeErrorKind::UnboundVariable {
            name: name.to_string(),
            suggestions,
            boundary,
        });
    }

    fn builtin_value(&mut self, entry: &Decl, at: Option<Span>) -> (Ty, Val<Inferring>) {
        if entry.is_boundary() {
            let (params, tail) = self.declared_spine(entry);
            let boundary = self.note_boundary_call(entry, &tail, entry.fixed_arity());
            let ty = Ty::Thunk(Box::new(CompTy::arrows(params, tail)));
            if let Some((result, missing)) = boundary {
                let node = readback::boundary_block(
                    &entry.name,
                    missing,
                    result,
                    at,
                    &mut self.synth_counter,
                );
                return (ty, node);
            }
            return (ty, Val::Variable(entry.name.clone().into()));
        }
        let ty = self.solver.instantiate(entry.scheme());
        (ty, Val::Variable(entry.name.clone().into()))
    }

    // The solution, not this read, decides whether the value is unit.
    fn note_unit_read(&mut self, name: &str, ty: &Ty) {
        if let Some(callee) = self.env.lookup_called(name) {
            self.solver.unit_read(
                UnitCall {
                    name: name.to_string(),
                    callee: callee.to_string(),
                },
                ty,
            );
        }
    }

    fn protected_val(&mut self, value: &Val, site: &Arc<Site>) -> (Ty, Val<Inferring>) {
        let (have, value) = self.val(value, None);
        let want = self.solver.contract(site);
        self.solver.equate(&have, &want, Reason::ProtectContract);
        (
            want,
            Val::Protected {
                value: Box::new(value),
                site: Membrane::Kept(Arc::clone(site)),
            },
        )
    }

    /// `at` places boundary blocks without changing the diagnostic span.
    pub(in crate::typecheck) fn val(
        &mut self,
        val: &Val,
        at: Option<Span>,
    ) -> (Ty, Val<Inferring>) {
        match val {
            Val::Protected { value, site } => self.protected_val(value, site),
            Val::Unit => (Ty::Unit, Val::Unit),
            Val::String(v) => (Ty::String, Val::String(v.clone())),
            Val::Int(v) => (Ty::Int, Val::Int(*v)),
            Val::Float(v) => (Ty::Float, Val::Float(*v)),
            Val::Bool(v) => (Ty::Bool, Val::Bool(*v)),
            Val::Variable(name) => {
                if crate::syntax::is_control_operator(name) {
                    self.solver.diagnose(TypeErrorKind::ControlOperatorAsValue {
                        name: name.to_string(),
                    });
                    (self.solver.fresh_ty(), Val::Unit)
                } else {
                    match self.env.lookup_binding(name).cloned() {
                        Some(scheme) => {
                            let ty = self.solver.instantiate(&scheme);
                            let imported = self.note_residual_use(name, &scheme, &ty);
                            self.note_unit_read(name, &ty);
                            let value = Val::Variable(name.clone());
                            let value = if imported {
                                Val::Protected {
                                    value: Box::new(value),
                                    site: Membrane::Imported(ty.clone()),
                                }
                            } else {
                                value
                            };
                            (ty, value)
                        }
                        // Rows are borrowed, not cloned: a cloned row's scheme cache dies with it.
                        None => match self.env.builtins.clone().value(name) {
                            Some(entry) => self.builtin_value(entry, at),
                            // Base frames are also handlers; name the builtin the user wrote.
                            None if self.env.builtins.get(name).is_some() => {
                                self.solver.diagnose(TypeErrorKind::BuiltinNotFirstClass {
                                    name: name.to_string(),
                                });
                                (self.solver.fresh_ty(), Val::Unit)
                            }
                            None if self.env.lookup_handler(name).is_some() => {
                                self.solver.diagnose(TypeErrorKind::HandlerNotFirstClass {
                                    name: name.to_string(),
                                });
                                (self.solver.fresh_ty(), Val::Unit)
                            }
                            None if LANGUAGE_CONSTANTS.iter().any(|&(c, _)| c == &**name) => {
                                (Ty::Bool, Val::Variable(name.clone()))
                            }
                            None => {
                                self.diagnose_unbound(name);
                                (self.solver.fresh_ty(), Val::Unit)
                            }
                        },
                    }
                }
            }
            Val::Thunk(comp) => {
                let (cty, comp) = self.scope(|this| this.comp(comp.shape()));
                (Ty::Thunk(Box::new(cty)), Val::thunk(Arc::new(comp)))
            }
            Val::List(elems) => {
                let elem = self.solver.fresh_ty();
                let nodes: Vec<_> = elems
                    .shape()
                    .iter()
                    .map(|entry| self.list_entry(entry, false, &elem))
                    .collect();
                (Ty::list(elem), Val::list(nodes))
            }
            Val::Record(entries) => {
                let (ty, entries) = self.record_val(entries.shape());
                (ty, Val::Record(entries))
            }
            Val::Map(entries) => {
                let (ty, entries) = self.map_val(entries.shape());
                (ty, Val::Map(entries))
            }
            Val::Variant { label, payload } => {
                // Construction leaves the row open for other alternatives.
                let (payload_ty, payload) = self.val(payload, None);
                let rest = self.solver.fresh_row();
                (
                    Ty::Variant(Row::Extend(
                        Label::Case(label.to_string()),
                        Box::new(payload_ty),
                        Box::new(rest),
                    )),
                    Val::Variant {
                        label: label.clone(),
                        payload: Box::new(payload),
                    },
                )
            }
        }
    }

    fn index(&mut self, target: &Val, key: &Spanned<Key>) -> (Ty, Prim<Inferring>) {
        if let Some(literal) = spell_literal(target) {
            if let Key::Data(value) = &key.item {
                self.val(value, None);
            }
            self.solver.diagnose(TypeErrorKind::IndexOnLiteral {
                literal,
                key: spell_key(&key.item),
            });
            return (
                self.solver.fresh_ty(),
                Prim::Index {
                    target: Val::Unit,
                    key: Spanned::with_span(key.span, Key::Position(0)),
                },
            );
        }
        let (target_ty, target) = self.val(target, None);
        let (ty, key_node) =
            self.with_span(key.span, |this| this.index_step(&target_ty, &key.item));
        (
            ty,
            Prim::Index {
                target,
                key: Spanned::with_span(key.span, key_node),
            },
        )
    }

    fn index_step(&mut self, current_ty: &Ty, key: &Key) -> (Ty, Key<Inferring>) {
        let elem = self.solver.fresh_ty();
        let key = match key {
            Key::Label(label) => {
                let tail = self.solver.fresh_row();
                let record = Ty::Record(Row::Extend(
                    Label::Field(label.to_string()),
                    Box::new(elem.clone()),
                    Box::new(tail),
                ));
                self.solver
                    .equate(current_ty, &record, Reason::RecordFieldRead);
                Key::Label(label.clone())
            }
            Key::Position(n) => {
                self.solver.equate(
                    current_ty,
                    &Ty::list(elem.clone()),
                    Reason::ListPositionRead,
                );
                Key::Position(*n)
            }
            Key::Data(value) => {
                let (key_ty, value) = self.val(value, None);
                self.solver
                    .equate(current_ty, &Ty::map(elem.clone()), Reason::MapKeyRead);
                self.solver
                    .equate(&key_ty, &Ty::String, Reason::MapIndexKey);
                Key::Data(value)
            }
        };
        (elem, key)
    }

    pub(in crate::typecheck) fn prim(&mut self, prim: &Prim) -> (Ty, Prim<Inferring>) {
        match prim {
            Prim::Binary(op, lhs, rhs) => self.binary(*op, lhs, rhs),
            Prim::Negate(value) => {
                let (ty, value) = self.val(value, None);
                let number = self.solver.fresh_kinded(Kind::NUMBER);
                self.solver.equate(&ty, &number, Reason::Negation);
                (number, Prim::Negate(value))
            }
            Prim::Not(value) => self.not(value),
            Prim::Index { target, key } => self.index(target, key),
            Prim::Update { base, fields } => self.update(base, fields),
            Prim::List(elems) => self.list(elems),
            Prim::Map(entries) => self.map(entries),
            Prim::Render(value) => {
                let (ty, value) = self.val(value, None);
                let scalar = self.solver.fresh_kinded(Kind::SCALAR);
                self.solver.equate(&ty, &scalar, Reason::Interpolation);
                (Ty::String, Prim::Render(value))
            }
            Prim::Concat(parts) => {
                let mut nodes = Vec::new();
                for value in parts {
                    let (ty, value) = self.val(value, None);
                    self.solver.equate(&ty, &Ty::String, Reason::Interpolation);
                    nodes.push(value);
                }
                (Ty::String, Prim::Concat(nodes.into()))
            }
            Prim::Decode(value) => {
                let (ty, value) = self.val(value, None);
                self.solver.equate(&ty, &Ty::Bytes, Reason::Capture);
                (Ty::String, Prim::Decode(value))
            }
        }
    }

    fn not(&mut self, val: &Val) -> (Ty, Prim<Inferring>) {
        let (ty, val) = self.val(val, None);
        self.solver.equate(&ty, &Ty::Bool, Reason::NotOperand);
        (Ty::Bool, Prim::Not(val))
    }

    fn binary(&mut self, op: BinaryOp, lhs: &Val, rhs: &Val) -> (Ty, Prim<Inferring>) {
        let (lhs_ty, lhs) = self.val(lhs, None);
        let (rhs_ty, rhs) = self.val(rhs, None);
        let why = Reason::BinaryOperands(op);
        let operand = match op {
            BinaryOp::Arith(_) => self.solver.fresh_kinded(Kind::NUMBER),
            BinaryOp::Compare(_) => self.solver.fresh_kinded(Kind::COMPARABLE),
            BinaryOp::Eq(_) => self.solver.fresh_kinded(Kind::DATA),
        };
        self.solver.equate(&operand, &lhs_ty, why.clone());
        self.solver.equate(&operand, &rhs_ty, why.clone());
        if op == BinaryOp::Arith(ArithOp::Mod) {
            self.solver.equate(&operand, &Ty::Int, why);
        }
        let ty = match op {
            BinaryOp::Eq(_) | BinaryOp::Compare(_) => Ty::Bool,
            BinaryOp::Arith(_) => operand,
        };
        (ty, Prim::Binary(op, lhs, rhs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CompareOp, EqOp};
    use crate::source::FileId;
    use crate::ty::{Scheme, UnitId};
    use crate::typecheck::builtins::{Convention, scheme};
    use crate::typecheck::env::TyEnv;
    use crate::typecheck::generate::tests::{assert_solved_type, solved};
    use crate::typecheck::solve::{ArgvBoundary, Solver};
    use std::borrow::Cow;

    fn generator() -> Generator {
        Generator::new(Solver::new(), TyEnv::new(), Vec::new())
    }

    fn at(start: u32) -> Span {
        Span::new(FileId::DUMMY, start, start + 1)
    }

    fn value(start: u32, value: Val) -> Spanned<Val> {
        Spanned::new(at(start), value)
    }

    #[test]
    fn variable_reads_preserve_imported_token_identity_and_share_residuals() {
        let mut solver = Solver::new();
        let residual = solver.fresh_ty();
        let stored = Arc::new(Scheme::mono(residual.clone()));
        let mut generator = Generator::new(
            solver,
            TyEnv::new(),
            vec![("imported".into(), Arc::clone(&stored))],
        );
        generator.env.bind("imported".into(), Arc::clone(&stored));
        let (first, node) = generator.val(&Val::Variable("imported".into()), None);
        assert!(matches!(
            node,
            Val::Protected {
                site: Membrane::Imported(_),
                ..
            }
        ));
        let (second, _) = generator.val(&Val::Variable("imported".into()), None);
        assert_eq!(generator.residual_uses.len(), 1);
        generator
            .solver
            .equate(&first, &Ty::String, Reason::Argument);
        generator
            .env
            .bind("imported".into(), Scheme::mono(Ty::Bool));
        let (shadow, node) = generator.val(&Val::Variable("imported".into()), None);
        assert!(matches!(node, Val::Variable(_)));
        let solution = solved(generator.solver);
        assert_eq!(solution.resolve_ty(&second), Ty::String);
        assert_eq!(solution.resolve_ty(&residual), Ty::String);
        assert_eq!(solution.resolve_ty(&shadow), Ty::Bool);
    }

    #[test]
    fn protected_values_import_the_contract_and_keep_the_original_site() {
        let solution = solved(Solver::new());
        let site = Arc::new(solution.site(&Ty::String, UnitId::fresh()));
        let mut generator = generator();
        let source = Val::Protected {
            value: Box::new(Val::String("text".into())),
            site: Arc::clone(&site),
        };
        let (ty, node) = generator.val(&source, None);
        let Val::Protected {
            site: Membrane::Kept(kept),
            ..
        } = node
        else {
            panic!("retained contract must stay on the value")
        };
        assert!(Arc::ptr_eq(&site, &kept));
        let solution = solved(generator.solver);
        assert_eq!(solution.resolve_ty(&ty), Ty::String);
    }

    #[test]
    fn boundary_values_eta_expand_at_the_requested_span_and_keep_one_result_type() {
        let mut generator = generator();
        let entry = Decl::new(
            Cow::Borrowed("decode-at"),
            Convention::Value,
            scheme::from_json_at,
            "",
            true,
        );
        generator.env.builtins.push(vec![entry].into());
        let (ty, node) =
            generator.region(|this| this.val(&Val::Variable("decode-at".into()), Some(at(7))));
        let Val::Thunk(block) = node else {
            panic!("boundary value is a thunk")
        };
        assert_eq!(block.shape().span, Some(at(7)));
        let crate::ir::CompKind::Lam { body, .. } = &block.shape().item else {
            panic!("declared argument is eta expanded")
        };
        let crate::ir::CompKind::Exec(exec) = &body.item else {
            panic!("boundary body is a saturated builtin call")
        };
        assert_eq!(exec.argv.len(), 1);
        let Some(result) = exec.site.clone() else {
            panic!("boundary result site")
        };
        let stored = generator.solver.generalise(&ty);
        let first = generator.solver.instantiate(&stored);
        let second = generator.solver.instantiate(&stored);
        let expected = |ty| Ty::Thunk(Box::new(CompTy::arrows([Ty::argv()], CompTy::pure(ty))));
        assert!(
            generator
                .solver
                .equate(&first, &expected(Ty::Int), Reason::Argument)
        );
        assert!(
            !generator
                .solver
                .equate(&second, &expected(Ty::Bool), Reason::Argument)
        );
        let (store, errors) = generator.solver.refused();
        assert_eq!(errors.len(), 1);
        assert_eq!(store.resolve_ty(&result), Ty::Int);
    }

    #[test]
    fn builtin_values_leave_declared_unknown_computations_unknown() {
        fn unknown(binders: &mut crate::ty::Binders) -> Ty {
            Ty::Thunk(Box::new(CompTy::Var(binders.comp())))
        }
        for boundary in [false, true] {
            let mut generator = generator();
            let entry = Decl::new(
                Cow::Borrowed("unknown"),
                Convention::Value,
                unknown,
                "",
                boundary,
            );
            let (ty, node) = generator.builtin_value(&entry, Some(at(1)));
            assert!(matches!(node, Val::Variable(_)));
            let wanted = Ty::Thunk(Box::new(CompTy::arrows([Ty::Int], CompTy::pure(Ty::Bool))));
            assert!(generator.solver.equate(&ty, &wanted, Reason::Argument));
            let solution = solved(generator.solver);
            assert_solved_type(&solution.scratch(), &ty, &wanted);
        }
    }

    #[test]
    fn unit_read_notes_wait_for_the_final_type() {
        let mut generator = generator();
        let ty = generator.solver.fresh_ty();
        generator
            .env
            .bind("result".into(), Scheme::mono(ty.clone()));
        generator.env.note_called("result".into(), "f".into());
        generator.with_span(Some(at(3)), |this| {
            let (read, _) = this.val(&Val::Variable("result".into()), None);
            this.solver
                .gate(&read, ArgvBoundary::InShell("echo".into()));
        });
        generator.solver.equate(&ty, &Ty::Unit, Reason::Argument);
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].pos, Some(at(3)));
        let call = errors[0].unit.as_ref().expect("unit read note");
        assert_eq!(call.name, "result");
        assert_eq!(call.callee, "f");
    }

    #[test]
    fn update_replaces_a_slot_type_but_preserves_the_other_fields() {
        let mut generator = generator();
        let base = Val::Record(FieldsNode::new(
            vec![
                ("change".into(), value(1, Val::Int(1))),
                ("keep".into(), value(2, Val::Bool(true))),
            ]
            .into(),
        ));
        let (ty, node) = generator.prim(&Prim::Update {
            base: value(0, base),
            fields: vec![("change".into(), value(3, Val::String("new".into())))].into(),
        });
        assert!(matches!(node, Prim::Update { .. }));
        let (changed, _) = generator.index_step(&ty, &Key::Label("change".into()));
        let (kept, _) = generator.index_step(&ty, &Key::Label("keep".into()));
        let solution = solved(generator.solver);
        assert_eq!(solution.resolve_ty(&changed), Ty::String);
        assert_eq!(solution.resolve_ty(&kept), Ty::Bool);
    }

    #[test]
    fn duplicate_static_keys_are_diagnosed_for_records_and_both_map_forms() {
        for form in 0..3 {
            let mut generator = generator();
            let entries = vec![
                ("same".into(), value(1, Val::Int(1))),
                ("same".into(), value(2, Val::Int(2))),
            ];
            match form {
                0 => {
                    generator.val(&Val::Record(FieldsNode::new(entries.into())), None);
                }
                1 => {
                    generator.val(&Val::Map(FieldsNode::new(entries.into())), None);
                }
                _ => {
                    let entries = entries
                        .into_iter()
                        .map(|(key, v): (Name, _)| {
                            MapEntryV::Entry(value(0, Val::String(key.as_ref().into())), v)
                        })
                        .collect();
                    generator.prim(&Prim::Map(entries));
                }
            }
            let errors = generator.solver.refused().1;
            assert_eq!(errors.len(), 1);
            assert!(
                matches!(&errors[0].kind, TypeErrorKind::DuplicateField { label } if label == "same")
            );
            assert_eq!(errors[0].pos, Some(if form == 2 { at(0) } else { at(2) }));
        }
    }

    #[test]
    fn list_and_map_spreads_share_the_element_equations() {
        let mut generator = generator();
        let (list, _) = generator.prim(&Prim::List(
            vec![
                ValListElem::Single(value(1, Val::Int(1))),
                ValListElem::Spread(value(2, Val::list(vec![value(3, Val::Int(2))]))),
            ]
            .into(),
        ));
        let spread = Val::Map(FieldsNode::new(
            vec![("b".into(), value(4, Val::Bool(false)))].into(),
        ));
        let (map, _) = generator.prim(&Prim::Map(
            vec![
                MapEntryV::Entry(value(5, Val::String("a".into())), value(6, Val::Bool(true))),
                MapEntryV::Spread(value(7, spread)),
            ]
            .into(),
        ));
        let solution = solved(generator.solver);
        assert_solved_type(&solution.scratch(), &list, &Ty::list(Ty::Int));
        assert_solved_type(&solution.scratch(), &map, &Ty::map(Ty::Bool));
    }

    #[test]
    fn index_shapes_are_equations_on_unknown_targets() {
        for (key, target) in [
            (
                Key::Label("field".into()),
                Ty::Record(Row::Extend(
                    Label::Field("field".into()),
                    Box::new(Ty::Int),
                    Box::new(Row::Empty),
                )),
            ),
            (Key::Position(0), Ty::list(Ty::Int)),
            (Key::Data(Val::String("key".into())), Ty::map(Ty::Int)),
        ] {
            let mut generator = generator();
            let unknown = generator.solver.fresh_ty();
            generator
                .env
                .bind("target".into(), Scheme::mono(unknown.clone()));
            let (result, node) = generator.prim(&Prim::Index {
                target: Val::Variable("target".into()),
                key: Spanned::new(at(2), key),
            });
            assert!(matches!(
                node,
                Prim::Index {
                    key: Spanned { span: Some(_), .. },
                    ..
                }
            ));
            assert!(generator.solver.equate(&unknown, &target, Reason::Argument));
            let solution = solved(generator.solver);
            assert_eq!(solution.resolve_ty(&result), Ty::Int);
        }
    }

    #[test]
    fn indexing_a_literal_still_checks_a_computed_key() {
        let mut generator = generator();
        generator.prim(&Prim::Index {
            target: Val::Int(2),
            key: Spanned::new(at(1), Key::Data(Val::Variable("missing".into()))),
        });
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 2);
        assert!(matches!(
            errors[0].kind,
            TypeErrorKind::UnboundVariable { .. }
        ));
        assert!(matches!(
            errors[1].kind,
            TypeErrorKind::IndexOnLiteral { .. }
        ));
    }

    #[test]
    fn primitive_expected_shapes_cover_numeric_boolean_and_text_operations() {
        for (prim, expected) in [
            (
                Prim::Binary(BinaryOp::Arith(ArithOp::Add), Val::Int(1), Val::Int(2)),
                Ty::Int,
            ),
            (
                Prim::Binary(BinaryOp::Compare(CompareOp::Lt), Val::Int(1), Val::Int(2)),
                Ty::Bool,
            ),
            (
                Prim::Binary(BinaryOp::Eq(EqOp::Eq), Val::Bool(true), Val::Bool(false)),
                Ty::Bool,
            ),
            (
                Prim::Negate(Val::Float(crate::data::Finite::new(1.5).unwrap())),
                Ty::Float,
            ),
            (Prim::Not(Val::Bool(false)), Ty::Bool),
            (Prim::Render(Val::Int(1)), Ty::String),
            (
                Prim::Concat(vec![Val::String("a".into()), Val::String("b".into())].into()),
                Ty::String,
            ),
        ] {
            let mut generator = generator();
            let (ty, _) = generator.prim(&prim);
            let solution = solved(generator.solver);
            assert_eq!(solution.resolve_ty(&ty), expected);
        }
        let mut generator = generator();
        let bytes = generator.solver.fresh_ty();
        generator
            .env
            .bind("bytes".into(), Scheme::mono(bytes.clone()));
        let (decoded, _) = generator.prim(&Prim::Decode(Val::Variable("bytes".into())));
        let solution = solved(generator.solver);
        assert_eq!(solution.resolve_ty(&bytes), Ty::Bytes);
        assert_eq!(solution.resolve_ty(&decoded), Ty::String);
    }

    #[test]
    fn modulo_requires_integers() {
        let mut generator = generator();
        generator.prim(&Prim::Binary(
            BinaryOp::Arith(ArithOp::Mod),
            Val::Float(1.into()),
            Val::Float(2.into()),
        ));
        let errors = generator.solver.refused().1;
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn unbound_reads_suggest_values_not_handlers() {
        let mut generator = generator();
        generator.env.bind("result".into(), Scheme::mono(Ty::Int));
        generator.env.bind_handler(
            "resalt".into(),
            Scheme::mono(Ty::Unit),
            crate::typecheck::env::HandlerOrigin::Alias,
        );
        generator.val(&Val::Variable("reslt".into()), None);
        let errors = generator.solver.refused().1;
        let TypeErrorKind::UnboundVariable { suggestions, .. } = &errors[0].kind else {
            panic!("unbound variable diagnosis")
        };
        assert!(suggestions.iter().any(|s| s == "result"));
        assert!(!suggestions.iter().any(|s| s == "resalt"));
    }
    #[test]
    fn an_unbound_variable_is_refused_where_it_is_read() {
        assert_eq!(super::super::tests::error_codes("echo $nope"), ["T0071"]);
        assert_eq!(
            super::super::tests::error_codes("let f = { |x| $y }"),
            ["T0071"]
        );
        assert_eq!(
            super::super::tests::error_codes("let a = 1\necho $a"),
            Vec::<&str>::new()
        );
        assert_eq!(
            super::super::tests::error_codes("let t = $true; if $t { echo y } else { echo n }"),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn a_map_literal_refuses_a_static_duplicate_key() {
        assert_eq!(
            super::super::tests::error_codes("return [a: 1, a: 2]"),
            ["T0022"]
        );
        assert_eq!(
            super::super::tests::error_codes("return [:, a: 1, a: 2]"),
            ["T0022"]
        );
        assert_eq!(
            super::super::tests::error_codes("let k = 'x'\nreturn [$k: 1, a: 1, a: 2]"),
            ["T0022"]
        );
    }
}
